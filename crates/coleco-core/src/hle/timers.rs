// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The BIOS timers: INIT_TIMER, TIME_MGR, REQUEST_SIGNAL, TEST_SIGNAL and
//! FREE_SIGNAL.
//!
//! A game gives the BIOS a table in RAM and asks for a SIGNAL: a countdown of
//! N calls to TIME_MGR (which it calls once a frame, usually from its NMI
//! handler), after which TEST_SIGNAL answers yes once. Donkey Kong Jr, Mr. Do!
//! and a dozen others pace everything with them, so before these were written
//! those games waited forever.
//!
//! The table's entries are three bytes: flags, then the count. Flags: bit 7
//! the signal is DONE, 6 it REPEATS, 5 the entry is FREE, 4 it is the LAST
//! entry, 3 its count is LONG (over 255). A short count sits in the entry as
//! count and reload; a long one sits in the entry, or, if it repeats, in a
//! four-byte block (count and reload) that the entry points at, allocated
//! from a second area the game gives, whose next free byte is kept at
//! `$73D5`. The table grows past its end when full, into whatever follows it.
//!
//! Each returns the real routine's time, summed from its instructions along
//! the path taken (T-states in the comments), since TIME_MGR and TEST_SIGNAL
//! run every frame in the games that use them.
//!
//! As elsewhere, the real routines' quirks are kept: a short one-shot keeps
//! counting after it fires and fires again 256 calls later; a long one-shot
//! is not written back when it reaches zero, so it fires on every call from
//! then on; FREE_SIGNAL's compaction keeps the pointer it last moved as the
//! one it compares against, and moves the low byte of the length only.

use super::routines::{ram16, set_ram16, sz53p};
use crate::machine::ColecoBus;
use crate::z80::{Bus, Z80};

/// Where the timer table starts, and the next free byte of the long repeating
/// timers' data area.
const TABLE: u16 = 0x73d3;
const DATA_NEXT: u16 = 0x73d5;

const DONE: u8 = 0x80;
const REPEAT: u8 = 0x40;
const FREE: u8 = 0x20;
const LAST: u8 = 0x10;
const LONG: u8 = 0x08;

/// A table with no LAST entry would send the real loops through all of
/// memory until some byte had bit 4 set; the host stops after this many.
const MAX_ENTRIES: u32 = 0x10000 / 3;

fn peek(bus: &mut ColecoBus, addr: u16) -> u8 {
    bus.peek(addr)
}

/// Flags as `BIT n,(HL)` leaves them: as for a register, except that bits 5
/// and 3 come from the high byte of the Z80's hidden MEMPTR, the address the
/// last jump, call or indirect load worked with, and not from the byte.
fn bit_mem_flags(carry: u8, n: u32, v: u8, memptr_hi: u8) -> u8 {
    let set = v & (1 << n) != 0;
    (carry & 0x01)
        | 0x10
        | (memptr_hi & 0x28)
        | if set { 0 } else { 0x44 }
        | if set && n == 7 { 0x80 } else { 0 }
}

/// The routines' own code page: MEMPTR's high byte after any of their jumps
/// or calls, all of which land in `$0Fxx`.
const CODE_PAGE: u8 = 0x0f;

/// INIT_TIMER (`$1FC7`): the table at HL, one entry, free and last; the long
/// timers' data area at DE.
pub fn init_timer(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let (table, data) = (cpu.hl(), cpu.de());
    set_ram16(bus, TABLE, table);
    bus.write(table, FREE | LAST);
    set_ram16(bus, DATA_NEXT, data);
    cpu.set_hl(data);
    cpu.set_de(table);
    16 + 10 + 4 + 16
}

/// TIME_MGR (`$1FD3`): one tick for every timer in use.
pub fn time_mgr(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let mut entry = ram16(bus, TABLE);
    let (mut a, mut de) = (cpu.a(), cpu.de());
    let mut carry = cpu.f & 0x01;
    // LD HL,(nn).
    let mut c = 16;
    for _ in 0..MAX_ENTRIES {
        let flags = peek(bus, entry);
        // BIT 5,(HL); CALL Z, taken or not; BIT 4,(HL).
        if flags & FREE == 0 {
            c += 12 + 17 + tick(bus, entry, &mut a, &mut de, &mut carry) + 12;
        } else {
            c += 12 + 10 + 12;
        }
        let flags = peek(bus, entry);
        if flags & LAST != 0 {
            // JR NZ to the RET (the trap's).
            c += 12;
            cpu.set_a(a);
            cpu.set_de(de);
            cpu.set_hl(entry);
            // The CALL before the test, taken or not, leaves MEMPTR on the
            // routine's page.
            cpu.f = bit_mem_flags(carry, 4, flags, CODE_PAGE);
            return c;
        }
        // JR NZ not taken, INC HL x3, JR.
        c += 7 + 18 + 12;
        entry = entry.wrapping_add(3);
    }
    cpu.set_hl(entry);
    c
}

/// One timer's tick (`$0F49`), and its time with its RET. `a`, `de` and
/// `carry` follow the registers the real one leaves: a long count's test is
/// `LD A,E; OR D`, which clears carry.
fn tick(bus: &mut ColecoBus, entry: u16, a: &mut u8, de: &mut u16, carry: &mut u8) -> i32 {
    let flags = peek(bus, entry);
    let count = entry.wrapping_add(1);
    if flags & LONG == 0 {
        let n = peek(bus, count).wrapping_sub(1);
        bus.write(count, n);
        if n != 0 {
            // PUSH HL, BIT 3, JR Z, INC HL, DEC (HL), JR NZ, POP HL, RET.
            return 11 + 12 + 12 + 6 + 11 + 12 + 10 + 10;
        }
        if flags & REPEAT != 0 {
            *a = peek(bus, entry.wrapping_add(2));
            bus.write(count, *a);
        }
        fire(bus, entry);
        // ... JR NZ not taken, POP HL, PUSH HL, BIT 6, then the reload or
        // not, SET 7, POP HL, RET.
        let head = 11 + 12 + 12 + 6 + 11 + 7 + 10 + 11 + 12;
        let reload = if flags & REPEAT != 0 { 7 + 12 + 7 + 6 + 7 + 6 + 10 + 11 } else { 12 };
        return head + reload + 15 + 10 + 10;
    }
    *carry = 0;
    // The count, in the entry or, for a repeating one, in its data block.
    let at = if flags & REPEAT == 0 { count } else { ram16(bus, count) };
    let n = ram16(bus, at).wrapping_sub(1);
    *de = n;
    *a = (n as u8) | (n >> 8) as u8;
    // PUSH HL, BIT 3, JR Z not taken, BIT 6, JR NZ, then the count's load,
    // DEC DE, LD A,E, OR D: through the pointer for a repeating one.
    let head = if flags & REPEAT == 0 {
        11 + 12 + 7 + 12 + 7 + 6 + 7 + 6 + 7 + 6 + 4 + 4
    } else {
        11 + 12 + 7 + 12 + 12 + 6 + 7 + 6 + 7 + 4 + 7 + 6 + 7 + 6 + 4 + 4
    };
    if n != 0 {
        set_ram16(bus, at, n);
        // JR NZ, LD (HL),D, DEC HL, LD (HL),E, JR, POP HL, RET.
        return head + 12 + 7 + 6 + 7 + 12 + 10 + 10;
    }
    if flags & REPEAT != 0 {
        let reload = ram16(bus, at.wrapping_add(2));
        *de = reload;
        set_ram16(bus, at, reload);
    }
    fire(bus, entry);
    // JR NZ not taken, the reload for a repeating one, POP HL, PUSH HL, JR,
    // SET 7, POP HL, RET.
    let reload = if flags & REPEAT != 0 { 6 + 7 + 6 + 7 + 12 + 7 + 6 + 7 } else { 0 };
    head + 7 + reload + 10 + 11 + 12 + 15 + 10 + 10
}

fn fire(bus: &mut ColecoBus, entry: u16) {
    let flags = peek(bus, entry);
    bus.write(entry, flags | DONE);
}

/// REQUEST_SIGNAL (`$1FCD`): a signal after HL ticks, repeating if A is not
/// zero. Takes the first free entry, adding one at the end if there is none,
/// and returns its number in A.
pub fn request_signal(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let repeat = cpu.a();
    let count = cpu.hl();
    let mut entry = ram16(bus, TABLE);
    let mut index: u8 = 0;
    // LD C,A, EX DE,HL, LD HL,(nn), XOR A, LD B,A.
    let mut c = 4 + 4 + 16 + 4 + 4;
    for _ in 0..MAX_ENTRIES {
        let flags = peek(bus, entry);
        // BIT 5,(HL).
        c += 12;
        if flags & FREE != 0 {
            c += 7 + take(bus, entry, count, repeat);
            break;
        }
        // JR Z, BIT 4,(HL).
        c += 12 + 12;
        if flags & LAST != 0 {
            // Full: a new free, last entry after this one, which is then
            // found free on the next pass. JR NZ, PUSH DE, PUSH HL, INC HL
            // x3, INC B, LD (HL),n, EX DE,HL, POP HL, RES 4, EX DE,HL,
            // POP DE, JR.
            bus.write(entry.wrapping_add(3), FREE | LAST);
            bus.write(entry, flags & !LAST);
            c += 12 + 11 + 11 + 18 + 4 + 10 + 4 + 10 + 15 + 4 + 10 + 12;
        } else {
            // JR NZ not taken, INC HL x3, INC B, JR.
            c += 7 + 18 + 4 + 12;
        }
        entry = entry.wrapping_add(3);
        index = index.wrapping_add(1);
    }
    cpu.set_a(index);
    cpu.set_bc(u16::from_be_bytes([index, repeat]));
    cpu.set_de(count);
    cpu.set_hl(entry);
    // Every path's last flag-setting instruction is an OR of the repeat flag
    // into an A of zero.
    cpu.f = sz53p(repeat);
    c
}

/// Set up a free entry. It stays marked free while it is filled in and is
/// released at the end, as the real one does it. Returns its time, to the
/// routine's end.
fn take(bus: &mut ColecoBus, entry: u16, count: u16, repeat: u8) -> i32 {
    let mut flags = (peek(bus, entry) & LAST) | FREE;
    let (lo, hi) = (count as u8, (count >> 8) as u8);
    // PUSH HL, LD A,(HL), AND, OR, LD (HL),A, XOR A, OR D; at the end POP HL,
    // RES 5, LD A,B.
    let mut c = 11 + 7 + 7 + 7 + 7 + 4 + 4 + 10 + 15 + 4;
    c += if hi == 0 {
        // JR NZ not taken, OR C, JR Z (or SET 6), INC HL, two LD (HL),E, JR.
        7 + 4 + if repeat != 0 { 7 + 15 } else { 12 } + 6 + 7 + 6 + 7 + 12
    } else if repeat == 0 {
        // JR NZ, SET 3, LD A,C, OR A, JR Z, then three INC HL, two stores, JR.
        12 + 15 + 4 + 4 + 12 + 18 + 14 + 12
    } else {
        // JR NZ, SET 3, LD A,C, OR A, JR Z not taken, and the data block.
        12 + 15 + 4 + 4 + 7 + 11 + 4 + 16 + 4 + 15 + 6 + 7 + 6 + 7 + 4 + 10 + 7 + 6 + 7 + 6 + 7 + 6 + 7 + 6 + 16 + 12
    };
    if hi == 0 {
        if repeat != 0 {
            flags |= REPEAT;
        }
        bus.write(entry, flags);
        bus.write(entry.wrapping_add(1), lo);
        bus.write(entry.wrapping_add(2), lo);
    } else if repeat == 0 {
        flags |= LONG;
        bus.write(entry, flags);
        set_ram16(bus, entry.wrapping_add(1), count);
    } else {
        flags |= LONG | REPEAT;
        bus.write(entry, flags);
        let block = ram16(bus, DATA_NEXT);
        set_ram16(bus, entry.wrapping_add(1), block);
        set_ram16(bus, block, count);
        set_ram16(bus, block.wrapping_add(2), count);
        set_ram16(bus, DATA_NEXT, block.wrapping_add(4));
    }
    let flags = peek(bus, entry);
    bus.write(entry, flags & !FREE);
    c
}

/// Where a walk to signal `n` stopped: the entry reached, what was left of
/// the count (C), whether it is signal `n` or the table ended first, and
/// MEMPTR's high byte there.
struct Found {
    entry: u16,
    left: u8,
    found: bool,
    memptr_hi: u8,
    /// T-states of the walk: LD C,A; LD HL,(nn); LD B,A; LD DE,3; OR A;
    /// JR Z; then per entry BIT 4,(HL); JR NZ; ADD HL,DE; DEC C; JR NZ.
    t: i32,
}

/// Walk from the table's start to signal `n`, stopping early at the LAST
/// entry. MEMPTR: the table pointer's load (`$73D4`) before the first test,
/// the loop's jump back after that, and on arrival the ADD HL,DE that
/// stepped onto the entry (the old HL plus one), or the jump for signal 0.
fn find(bus: &mut ColecoBus, n: u8) -> Found {
    let mut entry = ram16(bus, TABLE);
    let mut left = n;
    let mut memptr_hi = 0x73;
    let mut t = 4 + 16 + 4 + 10 + 4;
    if n == 0 {
        return Found { entry, left, found: true, memptr_hi: CODE_PAGE, t: t + 12 };
    }
    t += 7;
    loop {
        t += 12;
        if peek(bus, entry) & LAST != 0 {
            return Found { entry, left, found: false, memptr_hi, t: t + 12 };
        }
        t += 7 + 11 + 4;
        memptr_hi = (entry.wrapping_add(1) >> 8) as u8;
        entry = entry.wrapping_add(3);
        left = left.wrapping_sub(1);
        if left == 0 {
            return Found { entry, left, found: true, memptr_hi, t: t + 7 };
        }
        t += 12;
        memptr_hi = CODE_PAGE;
    }
}

/// TEST_SIGNAL (`$1FD0`): whether signal A has fired, in A and the Z flag.
/// Yes clears it, and frees a one-shot.
pub fn test_signal(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let n = cpu.a();
    let at = find(bus, n);
    let mut fired = false;
    let mut c = at.t;
    if at.found {
        let flags = peek(bus, at.entry);
        // BIT 5, JR NZ; then BIT 7, JR NZ.
        c += 12 + if flags & FREE != 0 { 12 } else { 7 + 12 + if flags & DONE != 0 { 12 } else { 7 } };
        if flags & FREE == 0 && flags & DONE != 0 {
            // BIT 6, JR NZ or SET 5, RES 7, LD A,1, OR A.
            c += 12 + if flags & REPEAT != 0 { 12 } else { 7 + 15 } + 15 + 7 + 4;
            let flags = if flags & REPEAT == 0 { flags | FREE } else { flags };
            bus.write(at.entry, flags & !DONE);
            fired = true;
        }
    }
    if !fired {
        // XOR A, JR, OR A.
        c += 4 + 12 + 4;
    }
    let a = u8::from(fired);
    cpu.set_a(a);
    cpu.f = sz53p(a);
    cpu.set_bc(u16::from_be_bytes([n, at.left]));
    cpu.set_de(3);
    cpu.set_hl(at.entry);
    c
}

/// FREE_SIGNAL (`$1FCA`): release signal A. A long repeating one also gives
/// back its four-byte block: the data after it moves down, and the entries
/// pointing past it are moved with it.
pub fn free_signal(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let n = cpu.a();
    let at = find(bus, n);
    let entry = at.entry;
    // Every early return leaves A and B the signal number, C the count left,
    // DE 3, HL the entry, and the flags of the test that ended it.
    cpu.set_bc(u16::from_be_bytes([n, at.left]));
    cpu.set_de(3);
    cpu.set_hl(entry);
    let flags = peek(bus, entry);
    let test = |bit: u32| bit_mem_flags(0, bit, flags, at.memptr_hi);
    if !at.found {
        cpu.f = test(4);
        return at.t;
    }
    // BIT 5, JR NZ.
    if flags & FREE != 0 {
        cpu.f = test(5);
        return at.t + 12 + 12;
    }
    bus.write(entry, flags | FREE);
    // JR NZ not taken, SET 5, BIT 6, JR Z.
    let t = at.t + 12 + 7 + 15 + 12;
    if flags & REPEAT == 0 {
        cpu.f = test(6);
        return t + 12;
    }
    // BIT 3, JR Z.
    if flags & LONG == 0 {
        cpu.f = test(3);
        return t + 7 + 12 + 12;
    }
    let freed = ram16(bus, entry.wrapping_add(1));
    let mut de = freed;
    let mut e = ram16(bus, TABLE);
    // JR Z not taken, then INC HL, LD E, INC HL, LD D, PUSH DE, LD HL,(nn),
    // PUSH HL.
    let mut c = t + 7 + 12 + 7 + 6 + 7 + 6 + 7 + 11 + 16 + 11;
    for _ in 0..MAX_ENTRIES {
        let f = peek(bus, e);
        // BIT 4, JR NZ.
        c += 12;
        if f & LAST != 0 {
            c += 12;
            break;
        }
        // JR NZ not taken, BIT 5, JR NZ; the next-entry step (POP HL, INC HL
        // x3, PUSH HL, JR) below.
        c += 7 + 12;
        if f & FREE != 0 {
            c += 12;
        } else {
            // JR NZ not taken, LD A,(HL), AND, CP, JR NZ.
            c += 7 + 7 + 7 + 7;
            if f & (REPEAT | LONG) != REPEAT | LONG {
                c += 12;
            } else {
                // JR NZ not taken, INC HL x2, LD A,(HL), CP D, JR C.
                c += 7 + 12 + 7 + 4;
                let block = ram16(bus, e.wrapping_add(1));
                if block == de {
                    // Unreachable with a sound table (the freed entry is
                    // already marked free); the real one returns here with
                    // its stack off by two words.
                    return c;
                }
                if block > de {
                    // Through to the step down: LD D, DEC HL, LD E, DEC DE
                    // x4, two stores, INC HL, JR (and the low-byte compare
                    // when the high bytes match).
                    c += 7 + 7 + 7 + 6 + 7 + 24 + 7 + 6 + 7 + 12;
                    if (block >> 8) == (de >> 8) {
                        c += 6 + 7 + 4 + 7 + 7 + 6 - 7;
                    }
                    de = block.wrapping_sub(4);
                    set_ram16(bus, e.wrapping_add(1), de);
                } else {
                    c += 12;
                }
            }
        }
        c += 10 + 18 + 11 + 12;
        e = e.wrapping_add(3);
    }
    // LD B,0, OR A, POP HL, POP DE, PUSH HL, LD HL,(nn), SBC HL,DE, LD C,L,
    // LD L,E, LD H,D, INC HL x4, then after the move LD BC,8, SBC HL,BC,
    // LD (nn),HL, POP HL.
    c += 7 + 4 + 10 + 10 + 11 + 16 + 15 + 4 + 4 + 4 + 24 + 10 + 15 + 16 + 10;
    let end = ram16(bus, DATA_NEXT);
    let len = end.wrapping_sub(freed) & 0xff;
    let moved = if len == 0 { 0x10000 } else { len as u32 };
    for i in 0..moved {
        let v = peek(bus, freed.wrapping_add(4).wrapping_add(i as u16));
        bus.write(freed.wrapping_add(i as u16), v);
    }
    let borrow = u16::from(end < freed);
    let next = freed.wrapping_add(4).wrapping_add(moved as u16).wrapping_sub(8).wrapping_sub(borrow);
    set_ram16(bus, DATA_NEXT, next);
    c + 21 * moved as i32 - 5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::{Coleco, Firmware};

    /// A table at `$7100`, the long timers' data at `$7200`.
    fn machine() -> Coleco {
        let mut m = Coleco::new(Firmware::Hle, &[0u8; 0x2000]).unwrap();
        m.cpu.set_hl(0x7100);
        m.cpu.set_de(0x7200);
        init_timer(&mut m.cpu, &mut m.bus);
        m
    }

    fn request(m: &mut Coleco, count: u16, repeat: bool) -> u8 {
        m.cpu.set_hl(count);
        m.cpu.set_a(u8::from(repeat));
        request_signal(&mut m.cpu, &mut m.bus);
        m.cpu.a()
    }

    fn tick(m: &mut Coleco, n: u32) {
        for _ in 0..n {
            time_mgr(&mut m.cpu, &mut m.bus);
        }
    }

    fn test(m: &mut Coleco, signal: u8) -> bool {
        m.cpu.set_a(signal);
        test_signal(&mut m.cpu, &mut m.bus);
        assert_eq!(m.cpu.f & 0x40 == 0, m.cpu.a() == 1, "Z agrees with A");
        m.cpu.a() == 1
    }

    #[test]
    fn a_one_shot_fires_once_after_its_count_and_frees_itself() {
        let mut m = machine();
        assert_eq!(request(&mut m, 3, false), 0);
        tick(&mut m, 2);
        assert!(!test(&mut m, 0));
        tick(&mut m, 1);
        assert!(test(&mut m, 0));
        assert!(!test(&mut m, 0), "cleared by the test");
        assert_eq!(m.bus.ram[0x100] & FREE, FREE, "and freed");
        assert_eq!(request(&mut m, 5, false), 0, "so the next request reuses it");
    }

    #[test]
    fn a_long_repeating_signal_uses_a_data_block_and_reloads() {
        let mut m = machine();
        assert_eq!(request(&mut m, 300, true), 0);
        assert_eq!(&m.bus.ram[0x101..0x103], &[0x00, 0x72], "points at its block");
        assert_eq!(&m.bus.ram[0x200..0x204], &[0x2c, 0x01, 0x2c, 0x01]);
        assert_eq!(ram16(&mut m.bus, DATA_NEXT), 0x7204);
        tick(&mut m, 299);
        assert!(!test(&mut m, 0));
        tick(&mut m, 1);
        assert!(test(&mut m, 0));
        tick(&mut m, 300);
        assert!(test(&mut m, 0), "again after another 300");
    }

    /// The real one never writes a long one-shot's count back once it
    /// reaches zero, so from then on it fires on every tick.
    #[test]
    fn a_long_one_shot_fires_on_every_tick_after_zero() {
        let mut m = machine();
        let n = request(&mut m, 256, false);
        tick(&mut m, 256);
        assert_eq!(ram16(&mut m.bus, 0x7101), 1, "left at 1");
        m.bus.ram[0x100] &= !DONE;
        tick(&mut m, 1);
        assert_eq!(m.bus.ram[0x100] & DONE, DONE);
        assert!(test(&mut m, n));
    }

    #[test]
    fn a_full_table_grows_by_an_entry() {
        let mut m = machine();
        assert_eq!(request(&mut m, 9, false), 0);
        assert_eq!(request(&mut m, 9, true), 1);
        assert_eq!(request(&mut m, 9, false), 2);
        assert_eq!(m.bus.ram[0x100] & LAST, 0);
        assert_eq!(m.bus.ram[0x103] & LAST, 0);
        assert_eq!(m.bus.ram[0x106] & LAST, LAST);
        assert_eq!(m.bus.ram[0x103] & REPEAT, REPEAT);
    }

    /// Freeing a long repeating signal gives its block back: the blocks
    /// after it move down four bytes and their entries follow.
    #[test]
    fn freeing_a_long_repeater_compacts_the_data_area() {
        let mut m = machine();
        assert_eq!(request(&mut m, 0x0300, true), 0);
        assert_eq!(request(&mut m, 0x0400, true), 1);
        assert_eq!(request(&mut m, 9, false), 2);
        assert_eq!(&m.bus.ram[0x104..0x106], &[0x04, 0x72]);
        m.cpu.set_a(0);
        free_signal(&mut m.cpu, &mut m.bus);
        assert_eq!(m.bus.ram[0x100] & FREE, FREE);
        assert_eq!(&m.bus.ram[0x104..0x106], &[0x00, 0x72], "signal 1's block moved down");
        assert_eq!(&m.bus.ram[0x200..0x204], &[0x00, 0x04, 0x00, 0x04]);
        assert_eq!(ram16(&mut m.bus, DATA_NEXT), 0x7204);
    }

    #[test]
    fn a_signal_past_the_end_of_the_table_is_never_done() {
        let mut m = machine();
        request(&mut m, 1, false);
        tick(&mut m, 1);
        assert!(!test(&mut m, 4));
        assert!(test(&mut m, 0));
    }
}
