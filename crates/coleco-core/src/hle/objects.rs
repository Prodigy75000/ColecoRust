// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The BIOS's object system: ACTIVATE, PUTOBJ, INIT_WRITER and WRITER.
//!
//! A game describes each thing on screen as an OBJECT in its ROM: a pointer to
//! its GRAPHICS (patterns and frames, in ROM), a pointer to its STATUS (frame
//! number and a 16-bit pixel position, in RAM), and, for background objects,
//! a pointer to an OLD_SCREEN buffer that keeps what the object covers. The
//! game moves things by editing the status and calling PUTOBJ; the BIOS works
//! out the rest. Burgertime's chef and enemies are objects, which is why they
//! were missing before this was written.
//!
//! The low nibble of the graphics' first byte is the object's type:
//!
//! | type | what | written |
//! |---|---|---|
//! | 0 | semi-mobile: a rectangle of name-table cells, background restored behind it | yes |
//! | 1 | mobile: a 16 by 16 image at any pixel, built into background patterns | yes |
//! | 2, 3 | sprite: one hardware sprite (3 with the 32-pixel early clock) | yes |
//! | 4 and up | complex: a group of other objects moved together | yes |
//!
//! As everywhere in the HLE, the behaviour is the real BIOS's down to its
//! quirks, measured against it call by call by `routinediff`: a semi-mobile
//! object's saved background in VRAM is re-read and written back with counts
//! of height shifted by width and width shifted by height, not their product;
//! a column is sign-extended through an 8-bit shift; an old-screen buffer at
//! `$70xx` is saved to the work buffer but restored from `$70xx`. Games were
//! written against all of it.

use super::routines::{bit_flags, cp_flags, get_vram, mode2, put_vram, ram16, read_vram, set_ram16, sz53p, write_vram};
use crate::machine::ColecoBus;
use crate::z80::{Bus, Z80};

/// The cartridge header's pointer to the work buffer the object routines use
/// as scratch space.
const WORK_BUFFER: u16 = 0x8006;
/// DEFER_WRITES: when 1, PUTOBJ queues the object for WRITER instead.
const DEFER: u16 = 0x73c6;
/// The deferred-write queue: size, write index, read index, write pointer,
/// read pointer, start.
const QUEUE_SIZE: u16 = 0x73ca;
const QUEUE_IN: u16 = 0x73cb;
const QUEUE_OUT: u16 = 0x73cc;
const QUEUE_IN_PTR: u16 = 0x73cd;
const QUEUE_OUT_PTR: u16 = 0x73cf;
const QUEUE_START: u16 = 0x73d1;
/// An old-screen pointer with its top byte at or above this is in RAM; below
/// it, in VRAM. Bit 15 set means the object has no old screen at all.
const RAM_PAGE: u8 = 0x70;
/// How many objects one call may visit before the data is taken as garbage.
/// A garbage graphics byte reads as a complex object of up to 15 parts, each
/// of which may read as another: the real BIOS would recurse until its stack
/// ran through RAM and crash, which takes emulated time; here a routine runs
/// in one step, and 15 to the power of the nesting would never return. Real
/// games put a few dozen objects per call.
const MAX_VISITS: u32 = 4096;
/// And how deep they may nest, so the host's own stack holds: a real game's
/// complex objects are one or two levels deep.
const MAX_DEPTH: u32 = 32;

/// What is left of one call's allowance: visits, and the current depth.
struct Budget {
    visits: u32,
    depth: u32,
}

impl Budget {
    fn new() -> Self {
        Budget {
            visits: MAX_VISITS,
            depth: 0,
        }
    }

    /// Take one visit one level down, or refuse when either runs out.
    fn enter(&mut self) -> bool {
        if self.visits == 0 || self.depth >= MAX_DEPTH {
            return false;
        }
        self.visits -= 1;
        self.depth += 1;
        true
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }
}

fn peek(bus: &mut ColecoBus, addr: u16) -> u8 {
    bus.peek(addr)
}

/// One call of a VRAM table routine with its register arguments.
fn table_call(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    routine: fn(&mut Z80, &mut ColecoBus) -> i32,
    table: u8,
    index: u16,
    src: u16,
    count: u16,
) -> i32 {
    cpu.set_a(table);
    cpu.set_de(index);
    cpu.set_hl(src);
    cpu.iy = count;
    routine(cpu, bus) + 60
}

/// One call of a VRAM table routine with its register arguments: the
/// routine's own time only, for callers that count their CALL, the jump
/// table's JP and the RET themselves.
fn vram_body(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    routine: fn(&mut Z80, &mut ColecoBus) -> i32,
    table: u8,
    index: u16,
    src: u16,
    count: u16,
) -> i32 {
    cpu.set_a(table);
    cpu.set_de(index);
    cpu.set_hl(src);
    cpu.iy = count;
    routine(cpu, bus)
}

/// A call through the jump table from inside the BIOS: CALL, the slot's JP,
/// and the routine's RET.
const TABLE_CALL: i32 = 17 + 10 + 10;

/// One block move between RAM at `ram` and VRAM at `vram`.
fn block_call(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    routine: fn(&mut Z80, &mut ColecoBus) -> i32,
    ram: u16,
    vram: u16,
    count: u16,
) -> i32 {
    cpu.set_hl(ram);
    cpu.set_de(vram);
    cpu.set_bc(count);
    routine(cpu, bus) + 40
}

/// ACTIVATE (`$1FF7`): get the object at HL ready to be drawn. Its frame goes
/// to 0, its old screen is marked empty, and the first free pattern after its
/// own is noted in its status. With carry set, its patterns (and colours) are
/// also loaded into VRAM. A complex object activates each of its parts.
pub fn activate(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let f = cpu.f;
    let desc = cpu.hl();
    let load = f & 0x01 != 0;
    let gfx = ram16(bus, desc);
    let kind = peek(bus, gfx);
    let c = activate_at(cpu, bus, desc, load, &mut Budget::new());
    // A and F as each path leaves them. Most end by popping the AF pushed on
    // entry: the graphics' first byte, and the caller's flags. A sprite's
    // pattern load returns straight from PUT_VRAM. A mode 2 load ends on its
    // test of bit 5, with the carry of the address step before it clear.
    match kind & 0x0f {
        2 | 3 if load => {}
        0 if load && mode2(bus) => {
            cpu.set_a(kind);
            cpu.f = bit_flags(0, 5, kind);
        }
        _ => {
            cpu.set_a(kind);
            cpu.f = f;
        }
    }
    // Its RET is the trap's.
    c - 10
}

fn activate_at(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    desc: u16,
    load: bool,
    budget: &mut Budget,
) -> i32 {
    if !budget.enter() {
        return 0;
    }
    let c = activate_one(cpu, bus, desc, load, budget);
    budget.leave();
    c
}

fn activate_one(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    desc: u16,
    load: bool,
    budget: &mut Budget,
) -> i32 {
    let gfx = ram16(bus, desc);
    let status = ram16(bus, desc.wrapping_add(2));
    bus.write(status, 0);
    let kind = peek(bus, gfx);
    let byte = |bus: &mut ColecoBus, i: u16| peek(bus, gfx.wrapping_add(i));
    // The time, with the RET: the descriptor's four bytes, LD A,0, LD (BC),A,
    // LD A,(DE), PUSH AF, AND 15, and a DEC A and jump per type passed.
    let head = [101, 115, 129, 143, 159][(kind & 0x0f).min(4) as usize];
    // The registers each path leaves, since a game may read them: BC starts
    // as the status pointer, and marking an old screen in VRAM leaves the
    // BC and HL of the one-byte WRITE_VRAM that does it.
    head + match kind & 0x0f {
        0 => {
            let (mut c, vram) = mark_old_screen(bus, desc);
            // CALL $0572; then the first pattern and count to status + 5,
            // POP AF, JR NC.
            c += 17 + 64;
            let first = byte(bus, 1);
            let count = byte(bus, 2);
            bus.write(status.wrapping_add(5), first.wrapping_add(count));
            cpu.iy = status;
            cpu.set_bc(if vram { 0x00be } else { status });
            cpu.set_de(gfx.wrapping_add(2));
            cpu.set_hl(first as u16);
            if load {
                let patterns = ram16(bus, gfx.wrapping_add(3));
                // JR NC not taken, PUSH AF, LD A,(nn), BIT 1,A.
                c += 7 + 11 + 13 + 8 + load_patterns(cpu, bus, kind, first, count, patterns);
            } else {
                c += 12 + 10;
            }
            c
        }
        1 => {
            let (c, vram) = mark_old_screen(bus, desc);
            // CALL $0572, the two bytes to status + 5 and + 6, POP AF, RET.
            let c = c + 17 + 74 + 10;
            let (a, b) = (byte(bus, 2), byte(bus, 3));
            bus.write(status.wrapping_add(5), a);
            bus.write(status.wrapping_add(6), b);
            cpu.iy = status;
            cpu.set_bc(if vram { 0x00be } else { status });
            cpu.set_de(gfx.wrapping_add(3));
            cpu.set_hl(if vram { 0x0588 } else { desc.wrapping_add(5) });
            c
        }
        2 | 3 => {
            let first = byte(bus, 1);
            let patterns = ram16(bus, gfx.wrapping_add(2));
            let count = byte(bus, 4);
            bus.write(status.wrapping_add(5), first.wrapping_add(count));
            cpu.set_bc(count as u16);
            cpu.iy = count as u16;
            cpu.set_hl(patterns);
            cpu.set_de(first as u16);
            // The status + 5 byte and the registers for the load, POP AF;
            // RET NC, or LD A,1, CALL $1C27 and its RET, and the RET.
            188 + if load {
                5 + 7 + 17 + vram_body(cpu, bus, put_vram, 1, first as u16, patterns, count as u16) + 10 + 10
            } else {
                11
            }
        }
        4 => {
            let parts = kind >> 4;
            // The count from the graphics byte, the first part's pointer,
            // OR A, JR Z; per part POP AF, PUSH AF, PUSH HL, PUSH BC, EX DE,HL,
            // CALL $04A3, POP BC, POP HL, the next pointer, DJNZ; POP AF, RET.
            let mut c = 64 + if parts == 0 { 12 } else { 7 } + 10 + 10;
            for i in 0..parts as u16 {
                let part = ram16(bus, desc.wrapping_add(4 + 2 * i));
                c += 64 + activate_at(cpu, bus, part, load, budget) + 46 + if i + 1 == parts as u16 { 8 } else { 13 };
            }
            // The loop reads one pointer past the last part; C is still the
            // status pointer's low byte.
            let next = desc.wrapping_add(4 + 2 * parts as u16);
            cpu.set_bc(status & 0x00ff);
            cpu.set_de(ram16(bus, next));
            cpu.set_hl(next.wrapping_add(2));
            c
        }
        _ => {
            cpu.set_bc(status);
            cpu.set_de(gfx);
            cpu.set_hl(desc.wrapping_add(4));
            // DEC A, JR Z not taken, POP AF, RET.
            4 + 7 + 10 + 10
        }
    }
}

/// Mark a background object's old screen empty: `$80` in its first byte,
/// in RAM or VRAM as the pointer says. No old screen, no mark. Also says
/// whether it was VRAM, which changes the registers left behind.
/// The time is `$0572`'s with its RET: PUSH BC, POP IY, PUSH DE, the
/// pointer, BIT 7,D, JR NZ, and the store (or the one-byte WRITE_VRAM),
/// then POP DE, INC DE, RET.
fn mark_old_screen(bus: &mut ColecoBus, desc: u16) -> (i32, bool) {
    let old = ram16(bus, desc.wrapping_add(4));
    let hi = (old >> 8) as u8;
    if hi & 0x80 != 0 {
        return (64 + 12 + 26, false);
    }
    if hi >= RAM_PAGE {
        bus.write(old, 0x80);
        return (64 + 51 + 26, false);
    }
    let ctl = old.wrapping_add(0x4000);
    bus.vdp.write_control(ctl as u8);
    bus.vdp.write_control((ctl >> 8) as u8);
    bus.vdp.write_data(0x80);
    // JR C to the VRAM case, LD HL,nn, LD BC,1, CALL $1D01, its 173 and RET.
    (64 + 77 + 173 + 26, true)
}

/// A background object's patterns and colours into VRAM. In graphics mode 2
/// the pattern and colour tables come in three thirds of the screen, and
/// bits 7, 6 and 5 of the graphics' first byte say which thirds get a copy;
/// with bit 4 set, the colours are one byte per pattern, spread to eight.
fn load_patterns(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    kind: u8,
    first: u8,
    count: u8,
    patterns: u16,
) -> i32 {
    let bytes = count as u16 * 8;
    let colours = patterns.wrapping_add(bytes);
    let mut c = 0;
    if mode2(bus) {
        // JR Z not taken, the pointers and counts set up, POP BC, POP IY,
        // POP AF; per third BIT and JR Z (or CALL $0594), and the $100 step
        // (CALL $05E8) after the first two; RET.
        c += 7 + 149 + 17 + 60 + 17 + 60 + 10;
        let mut name = first as u16;
        for bit in [0x80, 0x40, 0x20] {
            c += 8;
            if kind & bit != 0 {
                // $0594: five PUSH, LD A,3, CALL $1C27 and RET, five POP,
                // five PUSH, BIT 4,A, JR NZ.
                c += 7 + 17 + 59 + 7 + 17 + 10 + 54 + 59 + 8;
                c += vram_body(cpu, bus, put_vram, 3, name, patterns, count as u16);
                if kind & 0x10 == 0 {
                    // ADD HL,BC, LD A,4, CALL $1C27 and RET, five POP, RET.
                    c += 7 + 11 + 7 + 17 + 10 + 54 + 10;
                    c += vram_body(cpu, bus, put_vram, 4, name, colours, count as u16);
                } else {
                    // JR NZ, the colour pointer, IY into HL; the spread;
                    // JR back, five POP, RET.
                    c += 12 + 44 + spread_colours(cpu, bus, name, colours, count as u16) + 12 + 54 + 10;
                }
            } else {
                c += 12;
            }
            name = name.wrapping_add(0x100);
        }
        // Each third's load saves and restores every register but IX, and
        // the name steps by `$100` after the first two thirds only.
        cpu.set_bc(bytes);
        cpu.iy = count as u16;
        cpu.set_de((first as u16).wrapping_add(0x200));
        cpu.set_hl(patterns);
        return c;
    }
    // JR Z, the patterns' setup, PUT_VRAM with its CALL and RET, the colour
    // groups' arithmetic, the second PUT_VRAM, POP AF, RET.
    c += 12 + 430 + 10;
    c += vram_body(cpu, bus, put_vram, 3, first as u16, patterns, count as u16);
    // Colour groups of eight patterns, first to last. The first group comes
    // from an arithmetic shift of the 8-bit pattern number, so a first
    // pattern of `$80` or more gives a "negative" group: the real BIOS's.
    let last = (first as u16 + count as u16).wrapping_sub(1) >> 3;
    let group = ((first as i8) >> 3) as u8 as u16;
    let groups = last.wrapping_sub(group).wrapping_add(1);
    c + vram_body(cpu, bus, put_vram, 4, group, colours, groups)
}

/// Mode 2 colours given one byte per pattern: each byte is written as that
/// pattern's eight colour rows, one pattern at a time, through the work
/// buffer.
fn spread_colours(cpu: &mut Z80, bus: &mut ColecoBus, name: u16, colours: u16, count: u16) -> i32 {
    let buffer = ram16(bus, WORK_BUFFER);
    let (mut name, mut src, mut left) = (name, colours, count);
    let mut c = 0;
    loop {
        let v = peek(bus, src);
        for i in 0..8 {
            bus.write(buffer.wrapping_add(i), v);
        }
        // PUSH HL, the byte, PUSH BC, the buffer's end, eight DEC HL, LD
        // (HL),A, DJNZ, PUSH DE, LD IY,1, LD A,4, CALL $1C27 and RET, the
        // pops and steps, DEC HL, LD A,H, OR L, JR NZ.
        c += 403 + vram_body(cpu, bus, put_vram, 4, name, buffer, 1);
        name = name.wrapping_add(1);
        src = src.wrapping_add(1);
        left = left.wrapping_sub(1);
        if left == 0 {
            return c - 5;
        }
    }
}

/// PUTOBJ (`$1FFA`): draw the object at IX as its status now says. B is
/// passed on to a complex object's parts and is a mobile object's drawing
/// flags. With DEFER_WRITES set, the object goes into WRITER's queue instead.
pub fn putobj(cpu: &mut Z80, bus: &mut ColecoBus) -> Option<i32> {
    let (desc, param) = (cpu.ix, (cpu.bc() >> 8) as u8);
    putobj_at(cpu, bus, desc, param, &mut Budget::new())
}

fn putobj_at(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    desc: u16,
    param: u8,
    budget: &mut Budget,
) -> Option<i32> {
    // LD A,(nn), CP 1, JR NZ; deferred, CALL $0623 and the queue with its
    // RET (the routine's own RET is the caller's).
    if peek(bus, DEFER) == 1 {
        return Some(13 + 7 + 7 + 17 + queue(cpu, bus, desc, param));
    }
    Some(13 + 7 + 12 + draw_object(cpu, bus, desc, param, budget)?)
}

/// Add an object to the deferred-write queue: its address and B, three
/// bytes, wrapping to the start after `size` entries. Registers as the real
/// one leaves them, since this is the whole routine for a deferring game.
fn queue(cpu: &mut Z80, bus: &mut ColecoBus, desc: u16, param: u8) -> i32 {
    let at = ram16(bus, QUEUE_IN_PTR);
    bus.write(at, desc as u8);
    bus.write(at.wrapping_add(1), (desc >> 8) as u8);
    bus.write(at.wrapping_add(2), param);
    let next = peek(bus, QUEUE_IN).wrapping_add(1);
    let size = peek(bus, QUEUE_SIZE);
    cpu.f = cp_flags(next, size);
    cpu.set_de(at.wrapping_add(3));
    if next == size {
        bus.write(QUEUE_IN, 0);
        let start = ram16(bus, QUEUE_START);
        set_ram16(bus, QUEUE_IN_PTR, start);
        cpu.set_a(0);
        cpu.set_hl(start);
    } else {
        bus.write(QUEUE_IN, next);
        set_ram16(bus, QUEUE_IN_PTR, at.wrapping_add(3));
        cpu.set_a(next);
        cpu.set_hl(QUEUE_SIZE);
    }
    // PUSH IX, LD HL,(nn), POP DE, three stores and INC HL, EX DE,HL, LD A,
    // INC A, LD HL,nn, CP (HL); then the wrap or the next pointer; RET.
    118 + if next == size { 7 + 7 + 13 + 16 + 16 + 12 } else { 12 + 13 + 20 } + 10
}

fn draw_object(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    desc: u16,
    param: u8,
    budget: &mut Budget,
) -> Option<i32> {
    if !budget.enter() {
        return Some(0);
    }
    let c = draw_one(cpu, bus, desc, param, budget);
    budget.leave();
    c
}

fn draw_one(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    desc: u16,
    param: u8,
    budget: &mut Budget,
) -> Option<i32> {
    let gfx = ram16(bus, desc);
    let kind = peek(bus, gfx);
    cpu.ix = desc;
    // From $06E3: LD H,(IX+1), LD L,(IX), LD A,(HL), LD C,A, AND, JP Z, and
    // a DEC A and JP Z per type passed. The time returned runs to the type's
    // last instruction before its RET.
    let dispatch = [66, 80, 94, 108, 118];
    Some(dispatch[(kind & 0x0f).min(4) as usize] + match kind & 0x0f {
        0 => semi_mobile(cpu, bus, desc, gfx),
        1 => mobile(cpu, bus, desc, gfx, param),
        2 => sprite(cpu, bus, desc, gfx, 0x08, 0xf9),
        3 => sprite(cpu, bus, desc, gfx, 0x20, 0xe1),
        _ => complex(cpu, bus, desc, gfx, kind, param, budget)?,
    })
}

/// A 16-bit pixel coordinate as a signed character cell, clamped to a byte.
fn cell(pixels: u16) -> u8 {
    let v = (pixels as i16) >> 3;
    v.clamp(-128, 127) as u8
}

/// `n` doubled `times - 1` times, as a DJNZ loop does it: 256 times for 0.
fn doubled(n: u16, times: u8) -> u16 {
    let mut v = n;
    let mut b = times;
    loop {
        b = b.wrapping_sub(1);
        if b == 0 {
            return v;
        }
        v = v.wrapping_add(v);
    }
}

/// A semi-mobile object: put back what was under it last time, keep what
/// is under it now, and draw its current frame.
fn semi_mobile(cpu: &mut Z80, bus: &mut ColecoBus, desc: u16, gfx: u16) -> i32 {
    let status = ram16(bus, desc.wrapping_add(2));
    let col = cell(ram16(bus, status.wrapping_add(1)));
    let row = cell(ram16(bus, status.wrapping_add(3)));
    let frame = peek(bus, status) as u16;
    let at = ram16(bus, gfx.wrapping_add(5).wrapping_add(2 * frame));
    let (w, h) = (peek(bus, at), peek(bus, at.wrapping_add(1)));
    let names = at.wrapping_add(2);
    let old = ram16(bus, desc.wrapping_add(4));
    let old_hi = (old >> 8) as u8;
    // The status into IY, both coordinates through $07E8, the frame's
    // record and its size, LD A,(IX+5), BIT 7,A.
    let mut c = 365 + clamp_time(ram16(bus, status.wrapping_add(1))) + clamp_time(ram16(bus, status.wrapping_add(3)));
    if old_hi & 0x80 != 0 {
        // Straight out of the drawing, IX as its PUT_VRAMs left it: JR Z not
        // taken, CALL $080B.
        return c + 7 + 17 + draw_cells(cpu, bus, names, row, col, w, h);
    }
    // JR Z, three PUSH, CP $70, and the page's jumps.
    c += 12 + 33 + 7 + match old_hi {
        RAM_PAGE => 12,
        h if h > RAM_PAGE => 7 + 7,
        _ => 7 + 12,
    };
    // What it covered last time, from RAM, or read back from VRAM into the
    // work buffer.
    let saved = if old_hi >= RAM_PAGE {
        // LD H,A, LD L,(IX+4), LD A,(HL), JR.
        c += 42;
        old
    } else {
        let buffer = ram16(bus, WORK_BUFFER);
        // LD HL,(nn), LD D,(IX+5), LD E,(IX+4), three PUSH, LD BC,4, CALL
        // $1D3E and its RET, POP HL, LD A,(HL), CP $80.
        c += 114 + 10 + 10 + 7 + 7;
        cpu.set_hl(buffer);
        cpu.set_de(old);
        cpu.set_bc(4);
        c += read_vram(cpu, bus);
        if peek(bus, buffer) != 0x80 {
            let (sw, sh) = (
                peek(bus, buffer.wrapping_add(2)),
                peek(bus, buffer.wrapping_add(3)),
            );
            let n = doubled(sh as u16, sw);
            // JR NZ, the size's load, the doubling, the count into BC, the
            // address past the header, CALL $1D3E and its RET, POP HL.
            c += 12 + 49 + doubling_time(sw) + 59 + 27 + 10;
            cpu.set_hl(buffer.wrapping_add(4));
            cpu.set_de(old.wrapping_add(4));
            cpu.set_bc(n);
            c += read_vram(cpu, bus);
        } else {
            // JR NZ not taken, POP DE, JR, POP HL.
            c += 7 + 10 + 12 + 10;
        }
        buffer
    };
    // LD A,(HL), CP $80, JR Z.
    c += 7 + 7;
    if peek(bus, saved) != 0x80 {
        let s = |bus: &mut ColecoBus, i: u16| peek(bus, saved.wrapping_add(i));
        let (sc, sr, sw, sh) = (s(bus, 0), s(bus, 1), s(bus, 2), s(bus, 3));
        // JR Z not taken, the header's four loads, PUSH IX, CALL $080B, POP IX.
        c += 7 + 59 + 15 + 17 + 14;
        c += draw_cells(cpu, bus, saved.wrapping_add(4), sr, sc, sw, sh);
    } else {
        c += 12;
    }
    // What it covers now. Three POP, three PUSH, the old screen's address,
    // LD A,$70, CP H, JR C (or the buffer's address), the header stored.
    c += 112 + if old_hi > RAM_PAGE { 12 } else { 7 + 16 } + 52;
    let keep = if old_hi > RAM_PAGE {
        old
    } else {
        ram16(bus, WORK_BUFFER)
    };
    for (i, v) in [col, row, w, h].into_iter().enumerate() {
        bus.write(keep.wrapping_add(i as u16), v);
    }
    // PUSH IX, CALL $0898, POP IX; three POP; PUSH IX, CALL $080B, POP IX;
    // LD D,(IX+5), LD A,$70, CP D.
    c += 15 + 17 + save_cells(cpu, bus, keep.wrapping_add(4), row, col, w, h) + 14;
    c += 30 + 15 + 17 + draw_cells(cpu, bus, names, row, col, w, h) + 14 + 30;
    // The drawing is wrapped in PUSH IX / POP IX here, then the old screen's
    // page is tested: RAM ends there, VRAM writes the buffer back.
    cpu.ix = desc;
    if old_hi < RAM_PAGE {
        let buffer = ram16(bus, WORK_BUFFER);
        let (sw, sh) = (
            peek(bus, buffer.wrapping_add(2)),
            peek(bus, buffer.wrapping_add(3)),
        );
        let n = doubled(sw as u16, sh);
        // JR Z and JR C not taken, the size through the alternates, the
        // doubling, the count and buffer, CALL $1D01 and its RET.
        c += 7 + 7 + 93 + doubling_time(sh) + 35 + 27;
        cpu.set_hl(buffer);
        cpu.set_de(old);
        cpu.set_bc(n);
        c += write_vram(cpu, bus);
    } else {
        c += if old_hi == RAM_PAGE { 12 } else { 7 + 12 };
        cpu.set_a(RAM_PAGE);
        cpu.f = cp_flags(RAM_PAGE, old_hi);
        cpu.set_de(u16::from_be_bytes([old_hi, cpu.de() as u8]));
    }
    c
}

/// Read `n` bytes at `src` into RAM at `dst`: from VRAM when `src` is below
/// `$7000`, as a plain copy otherwise.
fn fetch(cpu: &mut Z80, bus: &mut ColecoBus, src: u16, dst: u16, n: u16) -> i32 {
    if ((src >> 8) as u8) < RAM_PAGE {
        return block_call(cpu, bus, read_vram, dst, src, n);
    }
    copy(bus, src, dst, n)
}

/// Write `n` bytes from RAM at `src` to `dst`: into VRAM when `dst` is below
/// `$7000`, as a plain copy otherwise.
fn store(cpu: &mut Z80, bus: &mut ColecoBus, src: u16, dst: u16, n: u16) -> i32 {
    if ((dst >> 8) as u8) < RAM_PAGE {
        return block_call(cpu, bus, write_vram, src, dst, n);
    }
    copy(bus, src, dst, n)
}

/// LDIR: byte by byte, upward.
fn copy(bus: &mut ColecoBus, src: u16, dst: u16, n: u16) -> i32 {
    for i in 0..n {
        let v = peek(bus, src.wrapping_add(i));
        bus.write(dst.wrapping_add(i), v);
    }
    21 * n as i32 + 30
}

/// A mobile object: a 16 by 16 image at any pixel, drawn by rewriting the
/// patterns of a 3 by 3 block of cells. The background under the block is
/// copied into nine patterns of the object's own, the image is shifted to its
/// pixel offset and laid over them, and the block's names point at them. The
/// object owns 18 patterns from the name at descriptor + 6 and alternates
/// between two sets of nine each time it is put (bit 7 of its frame), so the
/// screen never shows a half-built one. Its old screen keeps the position and
/// the nine names it covered, to put back when it moves.
///
/// Work buffer layout, as the real routine uses it: 0 pixel row, 1 shift,
/// 2 colour, 3 flags (B, with bit 7 for mode 2), 4 frame, 5 first name, 6-16
/// the old record, 17-18 cell position, 19-27 the names under it, 28-99 nine
/// background patterns, 100-131 the image, 124-128 the frame's record over
/// its end, 132 on the colours.
fn mobile(cpu: &mut Z80, bus: &mut ColecoBus, desc: u16, gfx: u16, param: u8) -> i32 {
    let w = ram16(bus, WORK_BUFFER);
    let at = |i: u16| w.wrapping_add(i);
    let flags = if mode2(bus) {
        param | 0x80
    } else {
        param & 0x7f
    };
    bus.write(at(3), flags);
    let status = ram16(bus, desc.wrapping_add(2));
    let frame = peek(bus, status);
    bus.write(at(4), frame);
    bus.write(status, frame ^ 0x80);
    let x = ram16(bus, status.wrapping_add(1));
    bus.write(at(1), 8u8.wrapping_sub(x as u8 & 7));
    let col = cell(x);
    bus.write(at(0x11), col);
    let y = ram16(bus, status.wrapping_add(3));
    bus.write(at(0), y as u8 & 7);
    let row = cell(y);
    bus.write(at(0x12), row);
    let mut c = 800 + save_cells(cpu, bus, at(0x13), row, col, 3, 3);

    // The old record in, the names under the new block corrected where they
    // show the object's own patterns (from where it was last time) to what
    // was saved under those, and the new record out.
    let old = ram16(bus, desc.wrapping_add(4));
    let first = peek(bus, desc.wrapping_add(6));
    bus.write(at(5), first);
    c += fetch(cpu, bus, old, at(6), 11);
    let own = peek(bus, at(5));
    for i in 0..9 {
        let d = peek(bus, at(0x13 + i)).wrapping_sub(own);
        if d < 0x12 {
            let k = if d >= 9 { d - 9 } else { d };
            let v = peek(bus, at(8 + k as u16));
            bus.write(at(0x13 + i), v);
        }
        c += 60;
    }
    c += store(cpu, bus, at(0x11), old, 11);

    // The background's patterns and colours. Mode 2 reads each from the
    // third of the screen its row is in, and skips rows off the bottom.
    for i in 0..9u16 {
        let name = peek(bus, at(0x13 + i));
        let row_abs = ((i / 3) as u8).wrapping_add(peek(bus, at(0x12)));
        let pattern = at(0x1c + 8 * i);
        if peek(bus, at(3)) & 0x80 == 0 {
            c += table_call(cpu, bus, get_vram, 3, name as u16, pattern, 1);
            c += table_call(cpu, bus, get_vram, 4, (name >> 3) as u16, at(0x84 + i), 1);
        } else {
            let third = ((row_abs as i8) >> 3) as u8;
            if third < 3 {
                let index = u16::from_be_bytes([third, name]);
                c += table_call(cpu, bus, get_vram, 3, index, pattern, 1);
                c += table_call(cpu, bus, get_vram, 4, index, pattern.wrapping_add(0x68), 1);
            }
        }
        c += 200;
    }

    // The frame's record: four pattern names for the image's quarters (left
    // column top and bottom, then right) and a colour. Names below graphics
    // byte 1 come from the ROM table at graphics + 4, the rest from the table
    // at graphics + 2, in VRAM or RAM. Both index by name times 8 in 8 bits.
    let f = peek(bus, at(4));
    let record = ram16(
        bus,
        gfx.wrapping_add(6).wrapping_add(f.wrapping_add(f) as u16),
    );
    c += fetch(cpu, bus, record, at(0x7c), 5);
    let colour = peek(bus, at(0x80));
    bus.write(at(2), colour);
    let split = peek(bus, gfx.wrapping_add(1));
    let (upper, lower) = (
        ram16(bus, gfx.wrapping_add(2)),
        ram16(bus, gfx.wrapping_add(4)),
    );
    for k in 0..4u16 {
        let name = peek(bus, at(0x7c + k));
        let dst = at(0x64 + 8 * k);
        c += if name < split {
            copy(bus, lower.wrapping_add(name.wrapping_mul(8) as u16), dst, 8)
        } else {
            fetch(
                cpu,
                bus,
                upper.wrapping_add((name - split).wrapping_mul(8) as u16),
                dst,
                8,
            )
        };
    }

    // Sixteen pixel rows, each shifted to the pixel and laid over three
    // background patterns: ORed in with B's bit 0 clear, byte by nonzero
    // byte with it set. Mode 2 colours each row that got a byte, keeping the
    // background's colour in the low nibble unless B's bit 1 is set.
    let mut ix = at(0x1c).wrapping_add(peek(bus, at(0)) as u16);
    let mut src = at(0x64);
    for _ in 0..16 {
        let left = peek(bus, src);
        src = src.wrapping_add(1);
        let right = peek(bus, src.wrapping_add(15));
        let mut hl = u16::from_be_bytes([left, right]);
        let mut a: u8 = 0;
        let mut b = peek(bus, at(1));
        loop {
            b = b.wrapping_sub(1);
            if b & 0x80 != 0 {
                break;
            }
            let carry = (hl >> 15) as u8;
            hl <<= 1;
            a = (a << 1) | carry;
        }
        let bytes = [(0u16, a), (8, (hl >> 8) as u8), (16, hl as u8)];
        let fl = peek(bus, at(3));
        for (off, v) in bytes {
            let p = ix.wrapping_add(off);
            if fl & 0x01 == 0 {
                let old = peek(bus, p);
                bus.write(p, old | v);
            } else if v != 0 {
                bus.write(p, v);
            }
        }
        let fl = peek(bus, at(3));
        if fl & 0x80 != 0 {
            let colour = peek(bus, at(2));
            let keep = if fl & 0x02 == 0 { 0x0f } else { 0 };
            for (off, v) in bytes {
                if v != 0 {
                    let p = ix.wrapping_add(0x68 + off);
                    let old = peek(bus, p);
                    bus.write(p, (old & keep) | colour);
                }
            }
        }
        let n = peek(bus, at(0)).wrapping_add(1);
        bus.write(at(0), n);
        if n == 8 || n == 16 {
            ix = ix.wrapping_add(16);
        }
        ix = ix.wrapping_add(1);
        c += 400;
    }
    if peek(bus, at(3)) & 0x80 == 0 {
        let colour = peek(bus, at(2));
        let keep = if peek(bus, at(3)) & 0x02 == 0 {
            0x0f
        } else {
            0
        };
        for i in 0..9 {
            let p = at(0x84 + i);
            let old = peek(bus, p);
            bus.write(p, (old & keep) | colour);
        }
    }

    // The block's names: this half's nine patterns.
    let mut name = peek(bus, at(5));
    if peek(bus, at(4)) & 0x80 != 0 {
        name = name.wrapping_add(9);
    }
    let base = name;
    for i in 0..9 {
        bus.write(at(0x13 + i), name);
        name = name.wrapping_add(1);
    }

    // The patterns and colours out. Mode 1 writes one colour group per cell
    // on screen; its test reads the cell position at IY + 17, and IY is left
    // at 1 by the first PUT_VRAM, so from then on it reads the BIOS's own
    // bytes at `$0012`: the real routine's, kept.
    if peek(bus, at(3)) & 0x80 == 0 {
        c += table_call(cpu, bus, put_vram, 3, base as u16, at(0x1c), 9);
        let mut iy = w;
        for i in 0..9u8 {
            let group = peek(bus, at(0x13 + i as u16)) >> 3;
            let across = (i % 3).wrapping_add(peek(bus, iy.wrapping_add(0x11)));
            if across < 0x20 && (i / 3).wrapping_add(peek(bus, iy.wrapping_add(0x12))) < 0x18 {
                c += table_call(cpu, bus, put_vram, 4, group as u16, at(0x84 + i as u16), 1);
                iy = cpu.iy;
            }
            c += 150;
        }
    } else {
        for r in 0..3u8 {
            let row_abs = peek(bus, at(0x12)).wrapping_add(r);
            if row_abs < 0x18 {
                let index = u16::from_be_bytes([row_abs >> 3, base.wrapping_add(3 * r)]);
                let src = at(0x1c + 24 * r as u16);
                c += table_call(cpu, bus, put_vram, 3, index, src, 3);
                c += table_call(cpu, bus, put_vram, 4, index, src.wrapping_add(0x68), 3);
            }
        }
    }

    // What was under the old block back, unless it has not moved, then the
    // new block's names.
    let (old_col, old_row) = (peek(bus, at(6)), peek(bus, at(7)));
    if old_col != 0x80 && (peek(bus, at(0x11)), peek(bus, at(0x12))) != (old_col, old_row) {
        c += draw_cells(cpu, bus, at(8), old_row, old_col, 3, 3);
    }
    let (col, row) = (peek(bus, at(0x11)), peek(bus, at(0x12)));
    c + draw_cells(cpu, bus, at(0x13), row, col, 3, 3)
}

/// `$07E8` called directly: DE, a pixel coordinate, shifted to a cell and
/// clamped into E; D is left shifted, HL kept. The flags are those of the
/// ADD HL,DE that tests the range, over BIT 7,D and the last RR E.
pub fn clamp_entry(cpu: &mut Z80) -> i32 {
    let de = ((cpu.de() as i16) >> 3) as u16;
    let [d, e] = de.to_be_bytes();
    // The last RR E shifted out bit 2 of the original E.
    let rr = sz53p(e) | ((cpu.de() >> 2) & 1) as u8;
    let f = bit_flags(rr, 7, d);
    let bound: u16 = if d & 0x80 == 0 { 0xff80 } else { 0x0080 };
    cpu.f = super::routines::add16_flags(f, bound, de);
    let t = clamp_time(cpu.de()) - 10;
    cpu.set_de(u16::from_be_bytes([d, cell(cpu.de())]));
    t
}

/// `$08C0` called directly: signed row D and column E as a name-table
/// offset in DE, HL kept. Flags from its last ADD HL,DE over BIT 7,E.
pub fn offset_entry(cpu: &mut Z80) -> i32 {
    let [row, col] = cpu.de().to_be_bytes();
    let rows = ((row as i8 as i16) * 32) as u16;
    let mut f = bit_flags(cpu.f, 7, row);
    // Five ADD HL,HL: the carry of the last is what BIT 7,E keeps.
    let mut hl = (row as i8 as i16) as u16;
    for _ in 0..5 {
        f = super::routines::add16_flags(f, hl, hl);
        hl = hl.wrapping_add(hl);
    }
    f = bit_flags(f, 7, col);
    let de = (col as i8 as i16) as u16;
    cpu.f = super::routines::add16_flags(f, rows, de);
    cpu.set_de(cell_offset(row, col));
    offset_time(row, col)
}

/// `$080B` called directly: HL names, D row, E column, C width, B height.
pub fn draw_entry(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let [row, col] = cpu.de().to_be_bytes();
    let [h, w] = cpu.bc().to_be_bytes();
    let src = cpu.hl();
    // Its RET is the trap's.
    draw_cells(cpu, bus, src, row, col, w, h) - 10
}

/// `$07E8`'s time with its RET: PUSH HL, three SRA D and RR E, BIT 7,D,
/// JR NZ, LD HL,nn, ADD HL,DE, POP HL, and RET NC or C (or LD E,n and RET
/// when it clamps).
fn clamp_time(pixels: u16) -> i32 {
    let v = (pixels as i16) >> 3;
    match (v < 0, (-128..=127).contains(&v)) {
        (false, true) => 116,
        (false, false) => 127,
        (true, true) => 121,
        (true, false) => 132,
    }
}

/// `$08C0`'s time without its RET: PUSH HL, BIT 7,D and the high byte, LD
/// L,D, five ADD HL,HL, BIT 7,E and D's sign, ADD HL,DE, EX DE,HL, POP HL.
fn offset_time(row: u8, col: u8) -> i32 {
    let r = if row & 0x80 != 0 { 45 } else { 38 };
    let c = if col & 0x80 != 0 { 26 } else { 19 };
    r + 4 + 55 + 8 + c + 25
}

/// A DJNZ doubling loop's time: JR to the DJNZ, then ADD HL,HL and DJNZ per
/// doubling, and the DJNZ that falls through (256 turns for 0).
fn doubling_time(times: u8) -> i32 {
    12 + 24 * times.wrapping_sub(1) as i32 + 8
}

/// A cell position as a name-table offset: row times 32 plus column, both
/// signed.
fn cell_offset(row: u8, col: u8) -> u16 {
    ((row as i8 as i16) * 32 + col as i8 as i16) as u16
}

/// Draw a `w` by `h` block of names from `src` with its top left at a signed
/// cell position, clipped to the 32 by 24 screen. Leaves the registers the
/// real one does: B, C the size, D the row, HL the names, and, once drawn,
/// A = E = the height and IY the clipped width.
fn draw_cells(cpu: &mut Z80, bus: &mut ColecoBus, src: u16, row: u8, col: u8, w: u8, h: u8) -> i32 {
    let names = src;
    let mut offset = cell_offset(row, col);
    let mut src = src;
    cpu.set_bc(u16::from_be_bytes([h, w]));
    cpu.set_de(u16::from_be_bytes([row, col]));
    cpu.set_hl(names);
    // The time, with the RET: three PUSH, EXX, three POP, CALL $08C0 and its
    // RET, EXX, LD A,E, BIT 7,A; for a column on the left JR NZ, CP $20 and
    // RET NC; then ADD A,C, BIT 7,A, RET NZ, OR A, RET Z.
    let mut t = 108 + offset_time(row, col) + 12;
    t += if col & 0x80 == 0 { 14 } else { 12 };
    if col & 0x80 == 0 && col >= 0x20 {
        cpu.set_a(col);
        cpu.f = cp_flags(col, 0x20);
        return t + 11;
    }
    if col & 0x80 == 0 {
        t += 5;
    }
    t += 12;
    let end = col.wrapping_add(w);
    if end & 0x80 != 0 {
        cpu.set_a(end);
        cpu.f = bit_flags(u8::from(col as u16 + w as u16 > 0xff), 7, end);
        return t + 11;
    }
    t += 5 + 4;
    if end == 0 {
        cpu.set_a(0);
        cpu.f = sz53p(0);
        return t + 11;
    }
    // RET Z not taken, BIT 7,E, and the clipping for either side.
    t += 5 + 8;
    t += if col & 0x80 != 0 {
        7 + 176 + if end < 0x21 { 12 } else { 14 }
    } else {
        12 + 15 + match end {
            0x1f => 12 + 53,
            e if e < 0x1f => 19 + 53,
            _ => 94,
        }
    };
    // LD E,0.
    t += 7;
    let count = if col & 0x80 != 0 {
        let skip = col.wrapping_neg() as u16;
        src = src.wrapping_add(skip);
        offset = offset.wrapping_add(skip);
        end.min(0x20)
    } else if end <= 0x1f {
        w
    } else {
        0x20u8.wrapping_sub(col)
    };
    let mut c = t;
    let mut r: u8 = 0;
    loop {
        let y = row.wrapping_add(r);
        // LD A,D, ADD A,E, BIT 7,A, JR NZ; CP $18, JR NC; the pushes and
        // EXX around CALL $1C27 and its RET.
        c += 16;
        if y & 0x80 != 0 {
            c += 12;
        } else if y >= 0x18 {
            c += 7 + 7 + 12;
        } else {
            c += 21 + 186 + vram_body(cpu, bus, put_vram, 2, offset, src, count as u16);
        }
        src = src.wrapping_add(w as u16);
        offset = offset.wrapping_add(0x20);
        r = r.wrapping_add(1);
        // The pointers stepped through the alternates, INC E, LD A,E, CP B,
        // JR NZ; the RET at the end.
        c += 76 + 12 + if r == h { 7 + 10 } else { 12 };
        if r == h {
            cpu.set_a(h);
            cpu.f = cp_flags(h, h);
            cpu.set_bc(u16::from_be_bytes([h, w]));
            cpu.set_de(u16::from_be_bytes([row, h]));
            cpu.set_hl(names);
            cpu.iy = count as u16;
            return c;
        }
    }
}

/// Keep the `w` by `h` block of names at a cell position in `dst`. Not
/// clipped: rows off the screen read whatever the offset wraps to.
fn save_cells(cpu: &mut Z80, bus: &mut ColecoBus, dst: u16, row: u8, col: u8, w: u8, h: u8) -> i32 {
    let mut offset = cell_offset(row, col);
    let mut dst = dst;
    let mut left = h;
    // CALL $08C0 and its RET, the width into IY; per row the pushes, GET_VRAM
    // through the table, the pointers stepped, DEC B, JR NZ; the RET.
    let mut c = 17 + offset_time(row, col) + 10 + 53;
    loop {
        c += 208 + vram_body(cpu, bus, get_vram, 2, offset, dst, w as u16);
        dst = dst.wrapping_add(w as u16);
        offset = offset.wrapping_add(0x20);
        left = left.wrapping_sub(1);
        if left == 0 {
            return c + 7 + 10;
        }
        c += 12;
    }
}

/// A sprite object: its four attribute bytes built in the work buffer and
/// written to its slot, the byte at descriptor + 4. A position outside what
/// the sprite can show moves it off the left edge with a transparent colour.
/// `shift` and `edge` are the two sprite types' constants: 8 and `$F9` for
/// type 2, 32 and `$E1` for type 3, whose early-clock bit moves it 32 pixels.
fn sprite(cpu: &mut Z80, bus: &mut ColecoBus, desc: u16, gfx: u16, shift: u16, edge: u8) -> i32 {
    let buffer = ram16(bus, WORK_BUFFER);
    let status = ram16(bus, desc.wrapping_add(2));
    let slot = peek(bus, desc.wrapping_add(4)) as u16;
    let x = ram16(bus, status.wrapping_add(1));
    let y = ram16(bus, status.wrapping_add(3));
    // On screen: high byte 0, or `$FF` with a low byte the real test (a
    // signed compare) passes.
    let shown = |v: u16| {
        let (lo, hi) = (v as u8, (v >> 8) as u8);
        hi == 0 || (hi == 0xff && lo.wrapping_sub(edge) & 0x80 == 0)
    };
    // A coordinate's test: CP 0, JR Z; or CP $FF, JP NZ, LD A,C, CP n, JP M.
    let test = |v: u16| match (v >> 8) as u8 {
        0 => 12,
        0xff => 7 + 7 + 10 + 4 + 7 + 10,
        _ => 7 + 7 + 10,
    };
    // LD IY,(nn), the status pointer, LD DE,1, ADD HL,DE, X into BC, LD A,B,
    // CP 0, and X's test; then Y's load (INC HL, LD C, INC HL, LD B, LD A,B,
    // CP 0) and test.
    let mut c = 110 + test(x);
    if shown(x) {
        c += 37 + test(y);
    }
    if !shown(x) || !shown(y) {
        // Four PUSH, XOR A, LD D,0, LD E,(IX+4), POP HL, LD IY,1, GET_VRAM
        // through the table; the two stores through IY; the same again for
        // PUT_VRAM.
        c += 114 + TABLE_CALL + vram_body(cpu, bus, get_vram, 0, slot, buffer, 1);
        bus.write(buffer.wrapping_add(1), 0);
        bus.write(buffer.wrapping_add(3), 0x80);
        c += 134 + TABLE_CALL + vram_body(cpu, bus, put_vram, 0, slot, buffer, 1);
        return c;
    }
    // DEC HL x2, LD A,(HL), CP 0, JP Z; the colour and X for the early clock
    // or not (with its JP or JR on); the Y, the name, and PUT_VRAM, and the
    // JR to the RET.
    c += 36 + if (x >> 8) == 0 { 322 } else { 314 + if shift == 8 { 10 } else { 12 } } + 463 + TABLE_CALL + 12;
    let frame = peek(bus, status);
    let entry = ram16(bus, gfx.wrapping_add(5)).wrapping_add(frame.wrapping_shl(1) as u16);
    let colour = peek(bus, entry);
    let (x_out, colour) = if (x >> 8) != 0 {
        (x.wrapping_add(shift) as u8, colour | 0x80)
    } else {
        (x as u8, colour)
    };
    let name = peek(bus, entry.wrapping_add(1)).wrapping_add(peek(bus, gfx.wrapping_add(1)));
    for (i, v) in [y as u8, x_out, name, colour].into_iter().enumerate() {
        bus.write(buffer.wrapping_add(i as u16), v);
    }
    c + vram_body(cpu, bus, put_vram, 0, slot, buffer, 1)
}

/// A complex object: each part's status is set from the whole's position
/// plus the part's offset in the current frame, its frame from the frame's
/// list (keeping the part's bit 7), and then each part is put in turn.
fn complex(
    cpu: &mut Z80,
    bus: &mut ColecoBus,
    desc: u16,
    gfx: u16,
    kind: u8,
    param: u8,
    budget: &mut Budget,
) -> Option<i32> {
    let status = ram16(bus, desc.wrapping_add(2));
    let frame = peek(bus, status);
    let x = ram16(bus, status.wrapping_add(1));
    let y = ram16(bus, status.wrapping_add(3));
    let at = gfx
        .wrapping_add(1)
        .wrapping_add(frame.wrapping_mul(4) as u16);
    let mut frames = ram16(bus, at);
    let mut offsets = ram16(bus, at.wrapping_add(2));
    let parts = kind >> 4;
    // The frame's lists and the whole's position, the count from the
    // graphics byte, PUSH BC, PUSH IX.
    let mut c = 286;
    let mut n = parts;
    let mut list = desc.wrapping_add(4);
    loop {
        let part = ram16(bus, list);
        list = list.wrapping_add(2);
        let st = ram16(bus, part.wrapping_add(2));
        // The part's status into IY, its frame (BIT 7,(IY), JR Z, or SET),
        // X and Y through the alternates, and DJNZ.
        c += 371 + if peek(bus, st) & 0x80 != 0 { 15 } else { 12 } + if n == 1 { 8 } else { 13 };
        let f = peek(bus, frames) | (peek(bus, st) & 0x80);
        bus.write(st, f);
        frames = frames.wrapping_add(1);
        let dx = peek(bus, offsets) as u16;
        set_ram16(bus, st.wrapping_add(1), x.wrapping_add(dx));
        offsets = offsets.wrapping_add(1);
        let dy = peek(bus, offsets) as u16;
        set_ram16(bus, st.wrapping_add(3), y.wrapping_add(dy));
        offsets = offsets.wrapping_add(1);
        n = n.wrapping_sub(1);
        if n == 0 {
            break;
        }
    }
    // POP IY, LD BC,4, ADD IY,BC, POP DE.
    c += 49;
    let mut n = parts;
    let mut list = desc.wrapping_add(4);
    loop {
        let part = ram16(bus, list);
        list = list.wrapping_add(2);
        // The part's descriptor from the list, PUSH IY, PUSH DE, LD B,E,
        // CALL $1FFA through the table, POP DE, POP IY, DEC D, JR NZ.
        c += 178 + putobj_at(cpu, bus, part, param, budget)? + if n == 1 { 7 } else { 12 };
        n = n.wrapping_sub(1);
        if n == 0 {
            // It ends on DEC D to zero, with the part list's pointer in IY
            // and E still the parameter; carry is the last part's.
            cpu.set_de(param as u16);
            cpu.iy = list;
            cpu.f = 0x42 | (cpu.f & 0x01);
            return Some(c);
        }
    }
}

/// INIT_WRITER (`$1FE5`): a queue of A entries at HL for deferred writes.
pub fn init_writer(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let (size, at) = (cpu.a(), cpu.hl());
    bus.write(QUEUE_SIZE, size);
    bus.write(QUEUE_IN, 0);
    bus.write(QUEUE_OUT, 0);
    set_ram16(bus, QUEUE_START, at);
    set_ram16(bus, QUEUE_IN_PTR, at);
    set_ram16(bus, QUEUE_OUT_PTR, at);
    cpu.set_a(0);
    // LD (nn),A, LD A,0, two LD (nn),A, three LD (nn),HL.
    13 + 7 + 26 + 48
}

/// WRITER (`$1FE8`): draw every queued object, oldest first, with deferral
/// off while it does, then put deferral back as it was.
pub fn writer(cpu: &mut Z80, bus: &mut ColecoBus) -> Option<i32> {
    let f = cpu.f;
    let saved = peek(bus, DEFER);
    bus.write(DEFER, 0);
    // LD A,(nn), PUSH AF, LD A,0, LD (nn),A; at the end the last test (LD
    // A,(nn), LD HL,nn, CP (HL), JR Z), POP AF, LD (nn),A.
    let mut c = 44 + 30 + 12 + 10 + 13;
    let mut budget = Budget::new();
    // At most one lap of a 256-entry queue, however its indexes were left.
    for _ in 0..0x100 {
        let out = peek(bus, QUEUE_OUT);
        if out == peek(bus, QUEUE_IN) {
            break;
        }
        let at = ram16(bus, QUEUE_OUT_PTR);
        let desc = ram16(bus, at);
        let param = peek(bus, at.wrapping_add(2));
        // The test, JR Z not taken, the entry into IX and B, PUSH HL, CALL
        // $06E3 and its RET, LD A,(nn), INC A, LD HL,nn, CP (HL), JR NZ, the
        // next pointer or the wrap, JR.
        c += 30 + 7 + 98 + 17 + draw_object(cpu, bus, desc, param, &mut budget)? + 10 + 34;
        let next = peek(bus, QUEUE_OUT).wrapping_add(1);
        c += if next == peek(bus, QUEUE_SIZE) { 81 } else { 63 };
        if next == peek(bus, QUEUE_SIZE) {
            bus.write(QUEUE_OUT, 0);
            let start = ram16(bus, QUEUE_START);
            set_ram16(bus, QUEUE_OUT_PTR, start);
        } else {
            bus.write(QUEUE_OUT, next);
            set_ram16(bus, QUEUE_OUT_PTR, at.wrapping_add(3));
        }
    }
    bus.write(DEFER, saved);
    // It ends popping the AF it pushed with the old DEFER_WRITES in A, after
    // a last compare against the index at `$73CB`.
    cpu.set_a(saved);
    cpu.f = f;
    cpu.set_hl(QUEUE_IN);
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{Coleco, Firmware};

    /// Sprite attributes at `$1B00`, names at `$1800`, the work buffer at
    /// `$7100`; `data` placed in the cartridge at the given addresses.
    fn machine(data: &[(u16, &[u8])]) -> Coleco {
        let mut cart = vec![0u8; 0x2000];
        cart[6..8].copy_from_slice(&0x7100u16.to_le_bytes());
        for &(at, bytes) in data {
            let i = (at - 0x8000) as usize;
            cart[i..i + bytes.len()].copy_from_slice(bytes);
        }
        let mut m = Coleco::new(Firmware::Hle, &cart).unwrap();
        set_ram16(&mut m.bus, 0x73f2, 0x1b00);
        set_ram16(&mut m.bus, 0x73f6, 0x1800);
        m
    }

    fn put(m: &mut Coleco, desc: u16) -> Option<i32> {
        m.cpu.ix = desc;
        putobj(&mut m.cpu, &mut m.bus)
    }

    fn at16(m: &mut Coleco, addr: u16, v: u16) {
        set_ram16(&mut m.bus, addr, v);
    }

    /// A type 3 sprite: descriptor at `$8100` (graphics `$8200`, status
    /// `$7050`, slot 5), first pattern `$20`, frame 0 = colour `$0B`, name 2.
    fn sprite_machine() -> Coleco {
        machine(&[
            (0x8100, &[0x00, 0x82, 0x50, 0x70, 5]),
            (0x8200, &[0x03, 0x20, 0, 0, 0, 0x10, 0x82]),
            (0x8210, &[0x0b, 2]),
        ])
    }

    fn slot5(m: &Coleco) -> [u8; 4] {
        m.bus.vdp.vram[0x1b14..0x1b18].try_into().unwrap()
    }

    #[test]
    fn a_sprite_object_writes_its_attributes() {
        let mut m = sprite_machine();
        at16(&mut m, 0x7051, 0x0040);
        at16(&mut m, 0x7053, 0x0030);
        put(&mut m, 0x8100).unwrap();
        assert_eq!(slot5(&m), [0x30, 0x40, 0x22, 0x0b]);
    }

    /// Left of the screen, a type 3 sprite takes the early clock and moves
    /// 32 pixels right to make up for it; the real test for "left of the
    /// screen" is a signed compare of the low byte, which also passes
    /// `$FF00`, 256 pixels off. Further off, or right of 255, it is hidden:
    /// X 0 and a transparent colour, Y and name kept.
    #[test]
    fn a_sprite_off_the_left_edge_takes_the_early_clock_and_further_off_hides() {
        let mut m = sprite_machine();
        at16(&mut m, 0x7053, 0x0030);
        at16(&mut m, 0x7051, 0xfff0);
        put(&mut m, 0x8100).unwrap();
        assert_eq!(slot5(&m), [0x30, 0x10, 0x22, 0x8b]);
        at16(&mut m, 0x7051, 0xff00);
        put(&mut m, 0x8100).unwrap();
        assert_eq!(
            slot5(&m),
            [0x30, 0x20, 0x22, 0x8b],
            "the signed compare's quirk"
        );
        at16(&mut m, 0x7051, 0x0100);
        put(&mut m, 0x8100).unwrap();
        assert_eq!(slot5(&m), [0x30, 0x00, 0x22, 0x80]);
    }

    /// With DEFER_WRITES set nothing is drawn until WRITER, which draws the
    /// queue and leaves deferral as it found it.
    #[test]
    fn deferred_objects_wait_for_writer() {
        let mut m = sprite_machine();
        at16(&mut m, 0x7051, 0x0040);
        at16(&mut m, 0x7053, 0x0030);
        m.cpu.set_a(4);
        m.cpu.set_hl(0x7080);
        init_writer(&mut m.cpu, &mut m.bus);
        m.bus.ram[0x3c6] = 1;
        m.cpu.set_bc(0x0700);
        put(&mut m, 0x8100).unwrap();
        assert_eq!(slot5(&m), [0; 4], "queued, not drawn");
        assert_eq!(
            &m.bus.ram[0x080..0x083],
            &[0x00, 0x81, 0x07],
            "the object and B"
        );
        writer(&mut m.cpu, &mut m.bus).unwrap();
        assert_eq!(slot5(&m), [0x30, 0x40, 0x22, 0x0b]);
        assert_eq!(m.bus.ram[0x3c6], 1);
        assert_eq!(m.bus.ram[0x3cc], m.bus.ram[0x3cb], "queue drained");
    }

    /// A semi-mobile object of two names keeps what it covers in its old
    /// screen (in RAM at `$7200`) and puts it back when it moves.
    #[test]
    fn a_semi_mobile_object_restores_what_it_covered() {
        let mut m = machine(&[
            (0x8100, &[0x00, 0x82, 0x60, 0x70, 0x00, 0x72]),
            (0x8200, &[0x00, 0x40, 1, 0, 0, 0x30, 0x82]),
            (0x8230, &[2, 1, 0x41, 0x42]),
        ]);
        m.bus.vdp.vram[0x1840..0x1848].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        m.cpu.set_hl(0x8100);
        m.cpu.f = 0;
        activate(&mut m.cpu, &mut m.bus);
        assert_eq!(m.bus.ram[0x200], 0x80, "old screen marked empty");
        at16(&mut m, 0x7061, 8);
        at16(&mut m, 0x7063, 16);
        put(&mut m, 0x8100).unwrap();
        assert_eq!(
            &m.bus.vdp.vram[0x1840..0x1848],
            &[1, 0x41, 0x42, 4, 5, 6, 7, 8]
        );
        assert_eq!(&m.bus.ram[0x200..0x206], &[1, 2, 2, 1, 2, 3]);
        at16(&mut m, 0x7061, 40);
        put(&mut m, 0x8100).unwrap();
        assert_eq!(
            &m.bus.vdp.vram[0x1840..0x1848],
            &[1, 2, 3, 4, 5, 0x41, 0x42, 8]
        );
    }

    /// Garbage read as a complex object of 15 parts, each part itself, must
    /// still return: the host runs the routine in one step.
    #[test]
    fn a_self_containing_complex_object_returns() {
        let mut desc = vec![0x00, 0x82, 0x50, 0x70];
        for _ in 0..15 {
            desc.extend_from_slice(&[0x00, 0x81]);
        }
        let mut m = machine(&[(0x8100, &desc), (0x8200, &[0xf4])]);
        assert!(put(&mut m, 0x8100).is_some());
        m.cpu.set_hl(0x8100);
        m.cpu.f = 1;
        activate(&mut m.cpu, &mut m.bus);
    }

    #[test]
    fn cells_clamp_to_a_signed_byte_and_doubling_follows_djnz() {
        assert_eq!(cell(0x0048), 9);
        assert_eq!(cell(0xfff8), 0xff);
        assert_eq!(cell(0x0400), 0x7f);
        assert_eq!(cell(0xf000), 0x80);
        assert_eq!(doubled(3, 1), 3);
        assert_eq!(doubled(3, 3), 12);
        assert_eq!(doubled(1, 0), 0, "B of 0 loops 256 times");
    }
}
