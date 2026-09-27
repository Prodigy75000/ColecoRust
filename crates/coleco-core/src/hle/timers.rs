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
    80
}

/// TIME_MGR (`$1FD3`): one tick for every timer in use.
pub fn time_mgr(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let mut entry = ram16(bus, TABLE);
    let (mut a, mut de) = (cpu.a(), cpu.de());
    let mut carry = cpu.f & 0x01;
    let mut c = 40;
    for _ in 0..MAX_ENTRIES {
        let flags = peek(bus, entry);
        if flags & FREE == 0 {
            c += tick(bus, entry, &mut a, &mut de, &mut carry);
        }
        let flags = peek(bus, entry);
        c += 50;
        if flags & LAST != 0 {
            cpu.set_a(a);
            cpu.set_de(de);
            cpu.set_hl(entry);
            // The CALL before the test, taken or not, leaves MEMPTR on the
            // routine's page.
            cpu.f = bit_mem_flags(carry, 4, flags, CODE_PAGE);
            return c;
        }
        entry = entry.wrapping_add(3);
    }
    cpu.set_hl(entry);
    c
}

/// One timer's tick. `a`, `de` and `carry` follow the registers the real
/// one leaves: a long count's test is `LD A,E; OR D`, which clears carry.
fn tick(bus: &mut ColecoBus, entry: u16, a: &mut u8, de: &mut u16, carry: &mut u8) -> i32 {
    let flags = peek(bus, entry);
    let count = entry.wrapping_add(1);
    if flags & LONG == 0 {
        let n = peek(bus, count).wrapping_sub(1);
        bus.write(count, n);
        if n != 0 {
            return 60;
        }
        if flags & REPEAT != 0 {
            *a = peek(bus, entry.wrapping_add(2));
            bus.write(count, *a);
        }
        fire(bus, entry);
        return 90;
    }
    *carry = 0;
    // The count, in the entry or, for a repeating one, in its data block.
    let at = if flags & REPEAT == 0 { count } else { ram16(bus, count) };
    let n = ram16(bus, at).wrapping_sub(1);
    *de = n;
    *a = (n as u8) | (n >> 8) as u8;
    if n != 0 {
        set_ram16(bus, at, n);
        return 90;
    }
    if flags & REPEAT != 0 {
        let reload = ram16(bus, at.wrapping_add(2));
        *de = reload;
        set_ram16(bus, at, reload);
    }
    fire(bus, entry);
    120
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
    let mut c = 60;
    for _ in 0..MAX_ENTRIES {
        let flags = peek(bus, entry);
        if flags & FREE != 0 {
            c += take(bus, entry, count, repeat);
            break;
        }
        c += 60;
        if flags & LAST != 0 {
            // Full: a new free, last entry after this one, which is then
            // found free on the next pass.
            bus.write(entry.wrapping_add(3), FREE | LAST);
            bus.write(entry, flags & !LAST);
            c += 80;
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
/// released at the end, as the real one does it.
fn take(bus: &mut ColecoBus, entry: u16, count: u16, repeat: u8) -> i32 {
    let mut flags = (peek(bus, entry) & LAST) | FREE;
    let (lo, hi) = (count as u8, (count >> 8) as u8);
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
    120
}

/// Where a walk to signal `n` stopped: the entry reached, what was left of
/// the count (C), whether it is signal `n` or the table ended first, and
/// MEMPTR's high byte there.
struct Found {
    entry: u16,
    left: u8,
    found: bool,
    memptr_hi: u8,
}

/// Walk from the table's start to signal `n`, stopping early at the LAST
/// entry. MEMPTR: the table pointer's load (`$73D4`) before the first test,
/// the loop's jump back after that, and on arrival the ADD HL,DE that
/// stepped onto the entry (the old HL plus one), or the jump for signal 0.
fn find(bus: &mut ColecoBus, n: u8) -> Found {
    let mut entry = ram16(bus, TABLE);
    let mut left = n;
    let mut memptr_hi = 0x73;
    if n == 0 {
        return Found { entry, left, found: true, memptr_hi: CODE_PAGE };
    }
    loop {
        if peek(bus, entry) & LAST != 0 {
            return Found { entry, left, found: false, memptr_hi };
        }
        memptr_hi = (entry.wrapping_add(1) >> 8) as u8;
        entry = entry.wrapping_add(3);
        left = left.wrapping_sub(1);
        if left == 0 {
            return Found { entry, left, found: true, memptr_hi };
        }
        memptr_hi = CODE_PAGE;
    }
}

/// TEST_SIGNAL (`$1FD0`): whether signal A has fired, in A and the Z flag.
/// Yes clears it, and frees a one-shot.
pub fn test_signal(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let n = cpu.a();
    let at = find(bus, n);
    let mut fired = false;
    if at.found {
        let flags = peek(bus, at.entry);
        if flags & FREE == 0 && flags & DONE != 0 {
            let flags = if flags & REPEAT == 0 { flags | FREE } else { flags };
            bus.write(at.entry, flags & !DONE);
            fired = true;
        }
    }
    let a = u8::from(fired);
    cpu.set_a(a);
    cpu.f = sz53p(a);
    cpu.set_bc(u16::from_be_bytes([n, at.left]));
    cpu.set_de(3);
    cpu.set_hl(at.entry);
    80 + 40 * n as i32
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
        return 80;
    }
    if flags & FREE != 0 {
        cpu.f = test(5);
        return 80;
    }
    bus.write(entry, flags | FREE);
    if flags & REPEAT == 0 {
        cpu.f = test(6);
        return 100;
    }
    if flags & LONG == 0 {
        cpu.f = test(3);
        return 100;
    }
    let freed = ram16(bus, entry.wrapping_add(1));
    let mut de = freed;
    let mut e = ram16(bus, TABLE);
    let mut c = 200;
    for _ in 0..MAX_ENTRIES {
        let f = peek(bus, e);
        if f & LAST != 0 {
            break;
        }
        c += 80;
        if f & FREE == 0 && f & (REPEAT | LONG) == REPEAT | LONG {
            let block = ram16(bus, e.wrapping_add(1));
            if block == de {
                // Unreachable with a sound table (the freed entry is already
                // marked free); the real one returns here with its stack off
                // by two words.
                return c;
            }
            if block > de {
                de = block.wrapping_sub(4);
                set_ram16(bus, e.wrapping_add(1), de);
            }
        }
        e = e.wrapping_add(3);
    }
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
    c + 21 * moved as i32
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
