// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The BIOS sound driver, reimplemented.
//!
//! Games do not write the SN76489 themselves. They hand the BIOS a song
//! table and ask it to play entries from it; the driver walks note lists,
//! runs frequency and volume sweeps, and a routine the game calls once a
//! frame (PLAY_SONGS) puts the result on the chip. A perfect chip with no
//! driver is silence, which is why this exists.
//!
//! Written from what the real driver does to its data, learned by reading it
//! as the oracle, and held to it by `routinediff`. The data it keeps is the
//! interface games were written against:
//!
//! - `$7020`: the song table the game gave SOUND_INIT. Each song number N
//!   (from 1) has four bytes at `table + 4(N-1)`: its note list, and its
//!   ten-byte data area.
//! - a data area: `[0]` song number (low 6 bits, `$3E` for a special) with
//!   the channel in bits 7-6, or `$FF` when idle, `$00` ending the list of
//!   areas; `[1..2]` the next note; `[3]` frequency low; `[4]` attenuation
//!   (high nibble) and frequency high (low nibble); `[5]` duration; `[6..7]`
//!   the frequency sweep; `[8..9]` the volume sweep.
//! - `$7022`-`$7029`: the area now sounding on noise, tone 1, 2, 3, or the
//!   idle marker `$024C` (a `$FF` in the BIOS image).
//! - `$702A`: the noise control last written, so it is only rewritten on a
//!   change (a write restarts the noise generator).
//!
//! **Registers are part of the interface too.** Games call SOUND_MAN and
//! PLAY_SONGS from their NMI handlers every frame and some carry on with what
//! the driver left in the registers: Buck Rogers, Subroc, Spy Hunter and
//! Tarzan ran on the RAM-exact first version of this driver and did not
//! start. So the registers are carried through every path as the real
//! driver's instructions leave them, flags included, in `R`.
//!
//! **Time is part of it too.** SOUND_MAN and PLAY_SONGS run every frame, so
//! `R` also counts the Z80 T-states of the real instructions each path runs,
//! summed per helper from the real driver's code: a call returns after the
//! same time it took on the real BIOS, as `routinediff` confirms.
//!
//! **Special sounds call the cartridge.** A note of type 4, and every frame
//! of a song so marked, runs a routine in the game. Host code cannot call
//! guest code and wait, so at those points the driver does what the real one
//! does: it pushes the same values on the guest stack, jumps to the game's
//! routine, and resumes at a trap when the game returns, to the same
//! addresses the real driver resumes at. No state is kept on the host, so
//! save states need nothing new.

use super::routines::{add16_flags, and_flags, bit_flags, cp_flags, ram16, sz53p, Flow};
use crate::machine::ColecoBus;
use crate::z80::{Bus, Z80};

/// The idle-channel marker the channel pointers hold when nothing sounds.
pub const IDLE: u16 = 0x024c;

/// Where control comes back into the driver from game code: the continuation
/// traps. Each is where the real driver resumes.
pub const RESUME: [u16; 6] = [0x027b, 0x028d, 0x02f5, 0x039d, 0x03c6, 0x0461];

/// A song-area list with no `$00` end would send the real loops through all
/// of memory; the host stops after this many areas.
const MAX_AREAS: u32 = 0x1a00;

/// The registers the driver works in. SP is the CPU's own: pushes go to the
/// guest stack as the real driver's do.
#[derive(Clone, Copy)]
struct R {
    a: u8,
    f: u8,
    b: u8,
    c: u8,
    d: u8,
    e: u8,
    h: u8,
    l: u8,
    ix: u16,
    iy: u16,
    /// T-states run so far, the real driver's.
    t: i32,
}

impl R {
    fn load(cpu: &Z80) -> R {
        let [b, c] = cpu.bc().to_be_bytes();
        let [d, e] = cpu.de().to_be_bytes();
        let [h, l] = cpu.hl().to_be_bytes();
        R { a: cpu.a(), f: cpu.f, b, c, d, e, h, l, ix: cpu.ix, iy: cpu.iy, t: 0 }
    }

    fn store(&self, cpu: &mut Z80) {
        cpu.set_a(self.a);
        cpu.f = self.f;
        cpu.set_bc(self.bc());
        cpu.set_de(self.de());
        cpu.set_hl(self.hl());
        cpu.ix = self.ix;
        cpu.iy = self.iy;
    }

    fn af(&self) -> u16 {
        u16::from_be_bytes([self.a, self.f])
    }
    fn bc(&self) -> u16 {
        u16::from_be_bytes([self.b, self.c])
    }
    fn de(&self) -> u16 {
        u16::from_be_bytes([self.d, self.e])
    }
    fn hl(&self) -> u16 {
        u16::from_be_bytes([self.h, self.l])
    }
    fn set_af(&mut self, v: u16) {
        [self.a, self.f] = v.to_be_bytes();
    }
    fn set_bc(&mut self, v: u16) {
        [self.b, self.c] = v.to_be_bytes();
    }
    fn set_de(&mut self, v: u16) {
        [self.d, self.e] = v.to_be_bytes();
    }
    fn set_hl(&mut self, v: u16) {
        [self.h, self.l] = v.to_be_bytes();
    }

    /// `AND n` into A.
    fn and(&mut self, n: u8) {
        self.a &= n;
        self.f = and_flags(self.a);
    }
    /// `OR n` into A.
    fn or(&mut self, n: u8) {
        self.a |= n;
        self.f = sz53p(self.a);
    }
    /// `CP n`.
    fn cp(&mut self, n: u8) {
        self.f = cp_flags(self.a, n);
    }
    /// `RRCA`.
    fn rrca(&mut self) {
        let (a, f) = rrca(self.a, self.f);
        self.a = a;
        self.f = f;
    }
    /// `RLCA`.
    fn rlca(&mut self) {
        let carry = self.a >> 7;
        self.a = self.a.rotate_left(1);
        self.f = (self.f & 0xc4) | (self.a & 0x28) | carry;
    }
    /// `LD E,n; LD D,0; ADD HL,DE`.
    fn add_hl(&mut self, n: u8) {
        self.set_de(n as u16);
        self.f = add16_flags(self.f, self.hl(), n as u16);
        self.set_hl(self.hl().wrapping_add(n as u16));
    }
    /// `LD E,n; LD D,0; ADD IX,DE`.
    fn add_ix(&mut self, n: u8) {
        self.set_de(n as u16);
        self.f = add16_flags(self.f, self.ix, n as u16);
        self.ix = self.ix.wrapping_add(n as u16);
    }
}

/// `RRCA`: carry and bit 7 from bit 0; S, Z, P/V kept; H, N clear.
fn rrca(a: u8, f: u8) -> (u8, u8) {
    let r = a.rotate_right(1);
    (r, (f & 0xc4) | (r & 0x28) | (a & 0x01))
}

/// `RLC r`: all flags from the result, carry from bit 7.
fn rlc(v: u8) -> (u8, u8) {
    let r = v.rotate_left(1);
    (r, sz53p(r) | (v >> 7))
}

/// `INC r` from `v`: carry kept.
fn inc_flags(f: u8, v: u8) -> u8 {
    let r = v.wrapping_add(1);
    (f & 0x01)
        | (r & 0xa8)
        | if r == 0 { 0x40 } else { 0 }
        | if r & 0x0f == 0 { 0x10 } else { 0 }
        | if r == 0x80 { 0x04 } else { 0 }
}

/// `DEC r` from `v`: carry kept.
fn dec_flags(f: u8, v: u8) -> u8 {
    let r = v.wrapping_sub(1);
    (f & 0x01)
        | 0x02
        | (r & 0xa8)
        | if r == 0 { 0x40 } else { 0 }
        | if r & 0x0f == 0x0f { 0x10 } else { 0 }
        | if r == 0x7f { 0x04 } else { 0 }
}

/// `SUB n`: as CP, with bits 5 and 3 from the result.
fn sub_flags(a: u8, n: u8) -> u8 {
    (cp_flags(a, n) & !0x28) | (a.wrapping_sub(n) & 0x28)
}

/// `ADD A,n` or `ADC A,n`: result and flags.
fn add8(a: u8, n: u8, carry: u8) -> (u8, u8) {
    let sum = a as u16 + n as u16 + carry as u16;
    let r = sum as u8;
    let mut f = r & 0xa8;
    if r == 0 {
        f |= 0x40;
    }
    if (a & 0x0f) + (n & 0x0f) + carry > 0x0f {
        f |= 0x10;
    }
    if (a ^ n) & 0x80 == 0 && (a ^ r) & 0x80 != 0 {
        f |= 0x04;
    }
    if sum > 0xff {
        f |= 0x01;
    }
    (r, f)
}

fn peek(bus: &mut ColecoBus, addr: u16) -> u8 {
    bus.peek(addr)
}

fn push(cpu: &mut Z80, bus: &mut ColecoBus, v: u16) {
    cpu.sp = cpu.sp.wrapping_sub(2);
    let [lo, hi] = v.to_le_bytes();
    bus.write(cpu.sp, lo);
    bus.write(cpu.sp.wrapping_add(1), hi);
}

fn pop(cpu: &mut Z80, bus: &mut ColecoBus) -> u16 {
    let lo = bus.read(cpu.sp);
    let hi = bus.read(cpu.sp.wrapping_add(1));
    cpu.sp = cpu.sp.wrapping_add(2);
    u16::from_le_bytes([lo, hi])
}

fn set16(bus: &mut ColecoBus, addr: u16, v: u16) {
    let [lo, hi] = v.to_le_bytes();
    bus.write(addr, lo);
    bus.write(addr.wrapping_add(1), hi);
}

/// Song B's data area into IX (`$01C1`). The song number is scaled by
/// rotating it left twice; HL is left on the high byte of the area pointer
/// in the table, DE is the area, BC the scaled number.
fn area_of(r: &mut R, bus: &mut ColecoBus) {
    r.t += 16 + 12 + 4 + 7 + 16 + 11 + 7 + 6 + 7 + 11 + 14 + 10;
    let base = ram16(bus, 0x7020).wrapping_sub(2);
    let (c, _) = rlc(r.b);
    let (c, f) = rlc(c);
    r.b = 0;
    r.c = c;
    r.f = add16_flags(f, base, c as u16);
    let at = base.wrapping_add(c as u16);
    r.e = peek(bus, at);
    r.d = peek(bus, at.wrapping_add(1));
    r.set_hl(at.wrapping_add(1));
    r.ix = r.de();
}

/// SOUND_INIT (`$1FEE`): song table HL, B data areas (256 when 0). Every
/// area goes idle, a `$00` ends the list, all channels go idle, and the chip
/// is silenced.
pub fn sound_init(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let table = cpu.hl();
    let count = (cpu.bc() >> 8) as u8;
    set16(bus, 0x7020, table);
    let mut area = ram16(bus, table.wrapping_add(2));
    let n = if count == 0 { 256 } else { count as u32 };
    let mut f = cpu.f;
    for _ in 0..n {
        bus.write(area, 0xff);
        f = add16_flags(f, area, 10);
        area = area.wrapping_add(10);
    }
    bus.write(area, 0);
    for ch in 0..4 {
        set16(bus, 0x7022 + 2 * ch, IDLE);
    }
    bus.write(0x702a, 0xff);
    // The idle marker is the last thing loaded into HL; the flags are the
    // last ADD HL,DE of the loop's.
    cpu.set_hl(IDLE);
    cpu.set_de(10);
    cpu.set_bc(cpu.bc() & 0x00ff);
    cpu.f = f;
    // LD (nn),HL, INC HL x2, the area pointer's load, EX DE,HL, LD E, LD D;
    // per area LD (HL),n, ADD HL,DE, DJNZ; LD (HL),0, LD HL,nn, four
    // LD (nn),HL, LD A,n, LD (nn),A; then into TURN_OFF_SOUND.
    66 + 34 * n as i32 - 5 + 104 + turn_off_sound(cpu, bus)
}

/// TURN_OFF_SOUND (`$1FD6`): all four channels to full attenuation.
pub fn turn_off_sound(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    for v in [0x9f, 0xbf, 0xdf, 0xff] {
        bus.output(0xff, v);
    }
    cpu.set_a(0xff);
    // Four LD A,n and OUT.
    4 * (7 + 11)
}

/// Point each channel at the last active area claiming it (`$0295`). IX is
/// saved and restored around it.
fn reassign(r: &mut R, bus: &mut ColecoBus) {
    let saved = r.ix;
    // PUSH IX; LD HL,nn; four LD (nn),HL; LD B,1; CALL $01C1.
    r.t += 15 + 10 + 64 + 7 + 17;
    r.set_hl(IDLE);
    for ch in 0..4 {
        set16(bus, 0x7022 + 2 * ch, IDLE);
    }
    r.b = 1;
    area_of(r, bus);
    for _ in 0..MAX_AREAS {
        r.a = peek(bus, r.ix);
        r.cp(0);
        r.t += 19 + 7;
        if r.a == 0 {
            // JR Z; POP IX; RET.
            r.t += 12 + 14 + 10;
            break;
        }
        r.cp(0xff);
        r.t += 7 + 7;
        if r.a == 0xff {
            r.t += 12;
        } else {
            // JR Z not taken, LD A,(IX), AND, RLCA x3, LD E,A, LD D,0,
            // LD HL,nn, ADD HL,DE, PUSH IX, POP DE, LD (HL),E, INC HL, LD (HL),D.
            r.t += 7 + 19 + 7 + 12 + 4 + 7 + 10 + 11 + 15 + 10 + 7 + 6 + 7;
        }
        // LD E,10; LD D,0; ADD IX,DE; JR.
        r.t += 7 + 7 + 15 + 12;
        if r.a != 0xff {
            r.and(0xc0);
            r.rlca();
            r.rlca();
            r.rlca();
            let slot = r.a;
            r.set_hl(0x7022);
            r.add_hl(slot);
            r.set_de(r.ix);
            bus.write(r.hl(), r.e);
            r.set_hl(r.hl().wrapping_add(1));
            bus.write(r.hl(), r.d);
        }
        r.add_ix(10);
    }
    r.ix = saved;
}

/// Decrement the low nibble of `(HL)` in place, the high nibble kept
/// (`$0190`). A and the flags are those of the `SUB 1` on the nibble; Z when
/// it has just reached zero.
fn dec_nibble(r: &mut R, bus: &mut ColecoBus) -> bool {
    r.t += 7 + 18 + 7 + 11 + 18 + 10 + 10;
    let at = r.hl();
    let m = peek(bus, at);
    let lo = m & 0x0f;
    let v = lo.wrapping_sub(1);
    bus.write(at, (m & 0xf0) | (v & 0x0f));
    r.a = v;
    r.f = sub_flags(lo, 1);
    v == 0
}

/// Reload the low nibble of `(HL)` from its high nibble (`$01A6`); B is left
/// holding the high nibble.
fn reload_nibble(r: &mut R, bus: &mut ColecoBus) {
    r.t += 7 + 7 + 4 + 16 + 4 + 7 + 10;
    let at = r.hl();
    r.a = peek(bus, at);
    r.and(0xf0);
    r.b = r.a;
    for _ in 0..4 {
        r.rrca();
    }
    r.or(r.b);
    bus.write(at, r.a);
}

/// The volume sweep, once a frame (`$012F`). `[8]`: steps left (low nibble)
/// and the attenuation change (high nibble); `[9]`: frames per step, as a
/// counter (low) and its reload (high).
fn volume_sweep(r: &mut R, bus: &mut ColecoBus) {
    let ix = r.ix;
    r.a = peek(bus, ix.wrapping_add(8));
    r.cp(0);
    r.t += 19 + 7;
    if r.a == 0 {
        r.t += 11;
        return;
    }
    // RET Z not taken, PUSH IX, POP HL, LD D,0, LD E,9, ADD HL,DE, CALL.
    r.t += 5 + 15 + 10 + 7 + 7 + 11 + 17;
    r.set_hl(ix);
    r.add_hl(9);
    if !dec_nibble(r, bus) {
        r.t += 12 + 10;
        return;
    }
    // JR NZ not taken, CALL $01A6, then DEC HL and CALL $0190.
    r.t += 7 + 17;
    reload_nibble(r, bus);
    r.t += 6 + 17;
    r.set_hl(r.hl().wrapping_sub(1));
    if dec_nibble(r, bus) {
        // JR Z; LD (HL),0; RET.
        r.t += 12 + 10 + 10;
        bus.write(r.hl(), 0);
        return;
    }
    // JR Z not taken, the attenuation step, OR $FF, JR, RET.
    r.t += 7 + 96 + 12 + 10;
    r.a = peek(bus, r.hl());
    r.and(0xf0);
    r.e = r.a;
    r.set_hl(r.hl().wrapping_sub(4));
    r.a = peek(bus, r.hl());
    r.and(0xf0);
    let (a, f) = add8(r.a, r.e, 0);
    r.a = a;
    r.f = f;
    r.e = r.a;
    r.a = peek(bus, r.hl());
    r.and(0x0f);
    r.or(r.e);
    bus.write(r.hl(), r.a);
    r.or(0xff);
}

/// Duration and the frequency sweep, once a frame (`$00FC`). False (Z) when
/// the note is over. With no sweep (`[7]` zero), `[5]` counts frames down.
/// With one, `[6]` counts frames per step, `[5]` counts steps, and each step
/// adds the signed `[7]` to the frequency as a 16-bit add over `[3..4]`: a
/// borrow runs into the attenuation nibble, as it does on the real BIOS,
/// and bit 2 of `[4]` is then cleared.
fn duration_and_sweep(r: &mut R, bus: &mut ColecoBus) -> bool {
    let ix = r.ix;
    r.a = peek(bus, ix.wrapping_add(7));
    r.cp(0);
    r.t += 19 + 7;
    if r.a == 0 {
        // JR NZ not taken, LD A,(IX+5), DEC A.
        r.t += 7 + 19 + 4;
        let v = peek(bus, ix.wrapping_add(5));
        r.f = dec_flags(r.f, v);
        r.a = v.wrapping_sub(1);
        if r.a == 0 {
            r.t += 11;
            return false;
        }
        // RET Z not taken, LD (IX+5),A, RET.
        r.t += 5 + 19 + 10;
        bus.write(ix.wrapping_add(5), r.a);
        return true;
    }
    // JR NZ, PUSH IX, POP HL, LD E,6, LD D,0, ADD HL,DE, CALL $0190.
    r.t += 12 + 15 + 10 + 7 + 7 + 11 + 17;
    r.set_hl(ix);
    r.add_hl(6);
    if !dec_nibble(r, bus) {
        r.t += 12 + 10;
        return true;
    }
    // JR NZ not taken, CALL $01A6, DEC HL, LD A,(HL), DEC A.
    r.t += 7 + 17;
    reload_nibble(r, bus);
    r.t += 6 + 7 + 4;
    r.set_hl(r.hl().wrapping_sub(1));
    let v = peek(bus, r.hl());
    r.f = dec_flags(r.f, v);
    r.a = v.wrapping_sub(1);
    if r.a == 0 {
        r.t += 11;
        return false;
    }
    // RET Z not taken, LD (HL),A, DEC HL x2, LD A,(IX+7), CALL $01B1 and its
    // body (7 more for a negative step), INC HL, RES 2,(HL), OR $FF, RET.
    r.t += 5 + 7 + 12 + 19 + 17 + 81 + 6 + 15 + 7 + 10;
    if peek(bus, ix.wrapping_add(7)) & 0x80 != 0 {
        r.t += 2;
    }
    bus.write(r.hl(), r.a);
    r.set_hl(r.hl().wrapping_sub(2));
    // The signed step onto the 16-bit frequency (`$01B1`).
    r.a = peek(bus, ix.wrapping_add(7));
    r.b = if r.a & 0x80 != 0 { 0xff } else { 0 };
    let at = r.hl();
    let (lo, f) = add8(r.a, peek(bus, at), 0);
    bus.write(at, lo);
    let (hi, f) = add8(peek(bus, at.wrapping_add(1)), r.b, f & 0x01);
    bus.write(at.wrapping_add(1), hi);
    r.a = hi;
    r.f = f;
    r.set_hl(at.wrapping_add(1));
    let v = peek(bus, r.hl()) & !0x04;
    bus.write(r.hl(), v);
    r.or(0xff);
    true
}

/// What a step of the driver did: finished (the caller carries on), or
/// handed control to game code with the stack laid out for a resume, the
/// registers already in the CPU.
enum Step {
    Done,
    Suspended,
}

/// `$0478`: DE = IY = IX + DE.
fn ix_plus_de(r: &mut R) {
    r.t += 17 + 15 + 14 + 15 + 15 + 10 + 10;
    r.f = add16_flags(r.f, r.ix, r.de());
    r.iy = r.ix.wrapping_add(r.de());
    r.set_de(r.iy);
}

/// `LDDR` of BC bytes from HL down to DE.
fn lddr(r: &mut R, bus: &mut ColecoBus) {
    let mut last;
    loop {
        last = peek(bus, r.hl());
        bus.write(r.de(), last);
        r.set_hl(r.hl().wrapping_sub(1));
        r.set_de(r.de().wrapping_sub(1));
        r.set_bc(r.bc().wrapping_sub(1));
        if r.bc() == 0 {
            r.t += 16;
            break;
        }
        r.t += 21;
    }
    let k = r.a.wrapping_add(last);
    r.f = (r.f & 0xc1) | (k & 0x08) | if k & 0x02 != 0 { 0x20 } else { 0 };
}

/// Start the next note of the song in area IX (`$035F`). The caller has
/// pushed its return address, as the real CALL does; on `Done` it is still
/// there for the caller to take back.
fn next_note(cpu: &mut Z80, bus: &mut ColecoBus, r: &mut R) -> Step {
    let ix = r.ix;
    // LD A,(IX), AND, PUSH AF, LD (IX),n, LD L,(IX+1), LD H,(IX+2),
    // LD A,(HL), LD B,A, BIT 5,A.
    r.t += 19 + 7 + 11 + 19 + 19 + 19 + 7 + 4 + 8;
    r.a = peek(bus, ix);
    r.and(0x3f);
    push(cpu, bus, r.af());
    bus.write(ix, 0xff);
    r.l = peek(bus, ix.wrapping_add(1));
    r.h = peek(bus, ix.wrapping_add(2));
    let ptr = r.hl();
    r.a = peek(bus, ptr);
    r.b = r.a;
    r.f = bit_flags(r.f, 5, r.a);
    if r.a & 0x20 != 0 {
        // A rest: silent for the low five bits' frames. JR Z not taken,
        // PUSH BC, AND, INC HL, six stores through IX, JP $0461.
        r.t += 7 + 11 + 7 + 6 + 6 * 19 + 10;
        push(cpu, bus, r.bc());
        r.and(0x1f);
        r.set_hl(ptr.wrapping_add(1));
        set16(bus, ix.wrapping_add(1), r.hl());
        bus.write(ix.wrapping_add(4), 0xf0);
        bus.write(ix.wrapping_add(5), r.a);
        bus.write(ix.wrapping_add(7), 0);
        bus.write(ix.wrapping_add(8), 0);
        finish_note(cpu, bus, r);
        return Step::Done;
    }
    r.f = bit_flags(r.f, 4, r.a);
    // JR Z to here, BIT 4,A.
    r.t += 12 + 8;
    if r.a & 0x10 != 0 {
        r.f = bit_flags(r.f, 3, r.a);
        // JR Z not taken, BIT 3,A.
        r.t += 7 + 8;
        if r.a & 0x08 != 0 {
            // JR Z not taken, POP BC, CALL $025E; its RET at $039D.
            r.t += 7 + 10 + 17 + 10;
            // Repeat: start the song again. The real driver pops the song
            // number it pushed into BC and calls PLAY_IT, returning through
            // $039D.
            let v = pop(cpu, bus);
            r.set_bc(v);
            push(cpu, bus, 0x039d);
            if let Step::Suspended = play_it_song(cpu, bus, r) {
                return Step::Suspended;
            }
            pop(cpu, bus);
            return Step::Done;
        }
        // The end of the song: the area stays idle. JR Z, LD A,n, PUSH AF,
        // JP $0461.
        r.t += 12 + 7 + 11 + 10;
        r.a = 0xff;
        push(cpu, bus, r.af());
        finish_note(cpu, bus, r);
        return Step::Done;
    }
    r.and(0x3c);
    r.cp(4);
    // JR Z to here, AND, CP.
    r.t += 12 + 7 + 7;
    if r.a == 4 {
        // JR NZ not taken, POP IY, PUSH IY, PUSH BC, INC HL, LD E,(HL),
        // LD (IX+1),E, INC HL, LD D,(HL), LD (IX+2),D, INC HL, PUSH IY,
        // POP AF, PUSH DE, POP IY, LD DE,nn, PUSH DE, JP (IY).
        r.t += 7 + 14 + 15 + 11 + 6 + 7 + 19 + 6 + 7 + 19 + 6 + 15 + 10 + 11 + 14 + 10 + 11 + 8;
        // A special: the note is a routine in the game. Run it, then its
        // second entry seven bytes on, then finish at $0461.
        let song_af = pop(cpu, bus);
        push(cpu, bus, song_af);
        push(cpu, bus, r.bc());
        r.e = peek(bus, ptr.wrapping_add(1));
        bus.write(ix.wrapping_add(1), r.e);
        r.d = peek(bus, ptr.wrapping_add(2));
        bus.write(ix.wrapping_add(2), r.d);
        r.set_hl(ptr.wrapping_add(3));
        r.set_af(song_af);
        r.iy = r.de();
        r.set_de(0x03c6);
        push(cpu, bus, 0x03c6);
        r.store(cpu);
        cpu.pc = r.iy;
        return Step::Suspended;
    }
    // A tone: copy the note's bytes into the area's fields, last byte first
    // by LDDR; the note types differ in length.
    push(cpu, bus, r.bc());
    r.a = r.b;
    r.and(0x03);
    r.cp(0);
    // JR NZ to here, PUSH BC, LD A,B, AND, CP.
    r.t += 12 + 11 + 4 + 7 + 7;
    match r.a {
        0 => {
            // JR NZ not taken, INC HL x4, two stores, DEC HL, LD DE,nn,
            // the copy, two stores, JR $0461.
            r.t += 7 + 24 + 38 + 6 + 10 + 10 + 38 + 12;
            r.set_hl(ptr.wrapping_add(4));
            set16(bus, ix.wrapping_add(1), r.hl());
            r.set_hl(r.hl().wrapping_sub(1));
            r.set_de(5);
            ix_plus_de(r);
            r.set_bc(3);
            lddr(r, bus);
            bus.write(ix.wrapping_add(7), 0);
            bus.write(ix.wrapping_add(8), 0);
        }
        1 => {
            // JR NZ, CP, JR NZ not taken, LD E, LD D, ADD HL,DE, two
            // stores, DEC HL, INC E, LD BC,nn, a store, JR.
            r.t += 12 + 7 + 7 + 7 + 7 + 11 + 38 + 6 + 4 + 10 + 19 + 12;
            r.cp(1);
            r.set_hl(ptr);
            r.add_hl(6);
            set16(bus, ix.wrapping_add(1), r.hl());
            r.set_hl(r.hl().wrapping_sub(1));
            r.f = inc_flags(r.f, r.e);
            r.e = r.e.wrapping_add(1);
            ix_plus_de(r);
            r.set_bc(5);
            lddr(r, bus);
            bus.write(ix.wrapping_add(8), 0);
        }
        2 => {
            // JR NZ, CP, JR NZ, CP, JR NZ not taken, LD E, LD D, ADD HL,DE,
            // POP AF, PUSH AF, AND, JR NZ (or DEC HL), two stores, DEC HL,
            // LD E,9, LD BC,nn, LD A,0, LD (DE),A, DEC DE x2, LD C,3, JR.
            r.t += 12 + 7 + 12 + 7 + 7 + 7 + 7 + 11 + 10 + 11 + 7 + 38 + 6 + 7 + 10 + 7 + 7 + 12 + 7 + 12;
            r.t += if peek(bus, ptr) & 0xc0 != 0 { 12 } else { 7 + 6 };
            r.cp(1);
            r.cp(2);
            r.set_hl(ptr);
            r.add_hl(6);
            // POP AF; PUSH AF on the BC just pushed: A the header, F the C.
            let v = pop(cpu, bus);
            push(cpu, bus, v);
            r.set_af(v);
            r.and(0xc0);
            if r.a == 0 {
                r.set_hl(r.hl().wrapping_sub(1));
            }
            set16(bus, ix.wrapping_add(1), r.hl());
            r.set_hl(r.hl().wrapping_sub(1));
            r.e = 9;
            ix_plus_de(r);
            r.set_bc(2);
            lddr(r, bus);
            r.a = 0;
            bus.write(r.de(), 0);
            r.set_de(r.de().wrapping_sub(2));
            r.c = 3;
            lddr(r, bus);
        }
        _ => {
            // JR NZ, CP, JR NZ, CP, JR NZ, LD E, LD D, ADD HL,DE, two
            // stores, DEC HL, PUSH IX, POP IY, LD E,9, ADD IY,DE, PUSH IY,
            // POP DE, LD BC,nn.
            r.t += 12 + 7 + 12 + 7 + 12 + 7 + 7 + 11 + 38 + 6 + 15 + 14 + 7 + 15 + 15 + 10 + 10;
            r.cp(1);
            r.cp(2);
            r.set_hl(ptr);
            r.add_hl(8);
            set16(bus, ix.wrapping_add(1), r.hl());
            r.set_hl(r.hl().wrapping_sub(1));
            r.iy = ix;
            r.e = 9;
            r.f = add16_flags(r.f, r.iy, 9);
            r.iy = r.iy.wrapping_add(9);
            r.set_de(r.iy);
            r.set_bc(7);
            lddr(r, bus);
        }
    }
    finish_note(cpu, bus, r);
    Step::Done
}

/// The end of next_note (`$0461`): mark the area with its channel and song,
/// or `$3E` for a special, unless the song ended. Pops the two values
/// next_note pushed: the note header as AF, the song number as BC.
fn finish_note(cpu: &mut Z80, bus: &mut ColecoBus, r: &mut R) {
    // PUSH IX, POP HL, POP AF, POP BC, CP $FF.
    r.t += 15 + 10 + 10 + 10 + 7;
    r.set_hl(r.ix);
    let v = pop(cpu, bus);
    r.set_af(v);
    let v = pop(cpu, bus);
    r.set_bc(v);
    r.cp(0xff);
    if r.a == 0xff {
        r.t += 11;
        return;
    }
    // RET Z not taken, LD D,A, AND, CP, JR NZ (or LD B,n), LD A,D, AND,
    // OR B, LD (HL),A, RET.
    r.t += 5 + 4 + 7 + 7 + 4 + 7 + 4 + 7 + 10;
    r.t += if r.a & 0x3f == 4 { 7 + 7 } else { 12 };
    r.d = r.a;
    r.and(0x3f);
    r.cp(4);
    if r.a == 4 {
        r.b = 0x3e;
    }
    r.a = r.d;
    r.and(0xc0);
    r.or(r.b);
    bus.write(r.hl(), r.a);
}

/// PLAY_IT's body for song B (`$025E`): unless that song is already playing,
/// point its area at the start of its note list, start the first note, and
/// reassign the channels.
fn play_it_song(cpu: &mut Z80, bus: &mut ColecoBus, r: &mut R) -> Step {
    // PUSH BC, CALL $01C1, LD A,(IX), AND, POP BC, CP B.
    r.t += 11 + 17 + 19 + 7 + 10 + 4;
    push(cpu, bus, r.bc());
    area_of(r, bus);
    r.a = peek(bus, r.ix);
    r.and(0x3f);
    let v = pop(cpu, bus);
    r.set_bc(v);
    r.cp(r.b);
    if r.a == r.b {
        r.t += 11;
        return Step::Done;
    }
    // RET Z not taken, LD (IX),B, DEC HL x2, LD D,(HL), DEC HL, LD E,(HL),
    // two stores, CALL $035F; then CALL $0295 and RET.
    r.t += 5 + 19 + 12 + 7 + 6 + 7 + 38 + 17 + 17 + 10;
    bus.write(r.ix, r.b);
    let at = r.hl().wrapping_sub(2);
    r.d = peek(bus, at);
    r.e = peek(bus, at.wrapping_sub(1));
    r.set_hl(at.wrapping_sub(1));
    set16(bus, r.ix.wrapping_add(1), r.de());
    push(cpu, bus, 0x027b);
    if let Step::Suspended = next_note(cpu, bus, r) {
        return Step::Suspended;
    }
    pop(cpu, bus);
    reassign(r, bus);
    Step::Done
}

/// PLAY_IT (`$1FF1`): start song B.
pub fn play_it(cpu: &mut Z80, bus: &mut ColecoBus) -> Flow {
    let mut r = R::load(cpu);
    match play_it_song(cpu, bus, &mut r) {
        // The trap's RET is the routine's last, already counted.
        Step::Done => {
            r.store(cpu);
            Flow::Ret(r.t - 10)
        }
        Step::Suspended => Flow::Jump(r.t),
    }
}

/// SOUND_MAN (`$1FF4`), once a frame: every active area's sweeps and
/// duration, and the next note when one ends.
pub fn sound_man(cpu: &mut Z80, bus: &mut ColecoBus) -> Flow {
    let mut r = R::load(cpu);
    // LD B,1; CALL $01C1.
    r.t += 7 + 17;
    r.b = 1;
    area_of(&mut r, bus);
    sound_man_loop(cpu, bus, r)
}

/// SOUND_MAN's loop from area IX (`$0284`).
fn sound_man_loop(cpu: &mut Z80, bus: &mut ColecoBus, mut r: R) -> Flow {
    for _ in 0..MAX_AREAS {
        r.a = 0;
        let v = peek(bus, r.ix);
        r.cp(v);
        // LD A,0; CP (IX).
        r.t += 7 + 19;
        if v == 0 {
            // RET Z, less the trap's RET.
            r.t += 11 - 10;
            break;
        }
        // RET Z not taken, CALL $02D6.
        r.t += 5 + 17;
        push(cpu, bus, 0x028d);
        if let Step::Suspended = area_frame(cpu, bus, &mut r) {
            return Flow::Jump(r.t);
        }
        pop(cpu, bus);
        // LD E,10; LD D,0; ADD IX,DE; JR.
        r.t += 7 + 7 + 15 + 12;
        r.add_ix(10);
    }
    r.store(cpu);
    Flow::Ret(r.t)
}

/// One area's frame (`$02D6`), called with `$028D` pushed.
fn area_frame(cpu: &mut Z80, bus: &mut ColecoBus, r: &mut R) -> Step {
    let ix = r.ix;
    // `$01E9`: the area's song, and for a special its routine in HL.
    // CALL, LD A,(IX), CP $FF.
    r.t += 17 + 19 + 7;
    r.a = peek(bus, ix);
    r.cp(0xff);
    if r.a != 0xff {
        // RET Z not taken, AND, CP $3E.
        r.t += 5 + 7 + 7;
        r.and(0x3f);
        r.cp(0x3e);
        if r.a == 0x3e {
            // RET NZ not taken, PUSH IX, POP HL, INC HL, LD E,(HL),
            // INC HL, LD D,(HL), EX DE,HL, RET.
            r.t += 5 + 15 + 10 + 6 + 7 + 6 + 7 + 4 + 10;
            let routine = ram16(bus, ix.wrapping_add(1));
            r.set_de(ix.wrapping_add(2));
            r.set_hl(routine);
        } else {
            r.t += 11;
        }
    } else {
        r.t += 11;
    }
    r.cp(0xff);
    // CP $FF.
    r.t += 7;
    if r.a == 0xff {
        r.t += 11;
        return Step::Done;
    }
    r.cp(0x3e);
    // RET Z not taken, CP $3E.
    r.t += 5 + 7;
    if r.a == 0x3e {
        // A special's frame: its routine's second entry, returning to the
        // loop at $028D. JR NZ not taken, LD E, LD D, ADD HL,DE, JP (HL).
        r.t += 7 + 7 + 7 + 11 + 4;
        r.add_hl(7);
        r.store(cpu);
        cpu.pc = r.hl();
        return Step::Suspended;
    }
    // JR NZ, CALL $012F.
    r.t += 12 + 17;
    volume_sweep(r, bus);
    r.t += 17;
    if duration_and_sweep(r, bus) {
        // JR NZ; RET.
        r.t += 12 + 10;
        return Step::Done;
    }
    // JR NZ not taken, LD A,(IX), PUSH AF, CALL $035F.
    r.t += 7 + 19 + 11 + 17;
    r.a = peek(bus, ix);
    push(cpu, bus, r.af());
    push(cpu, bus, 0x02f5);
    if let Step::Suspended = next_note(cpu, bus, r) {
        return Step::Suspended;
    }
    pop(cpu, bus);
    area_tail(cpu, bus, r);
    // RET.
    r.t += 10;
    Step::Done
}

/// After a note change in SOUND_MAN (`$02F5`): if the area's song or
/// channel changed, reassign. Pops the old first byte area_frame pushed.
fn area_tail(cpu: &mut Z80, bus: &mut ColecoBus, r: &mut R) {
    let v = pop(cpu, bus);
    r.set_bc(v);
    r.a = peek(bus, r.ix);
    r.cp(r.b);
    // POP BC, LD A,(IX), CP B.
    r.t += 10 + 19 + 4;
    if r.a != r.b {
        // JR Z not taken, CALL $0295.
        r.t += 7 + 17;
        reassign(r, bus);
    } else {
        r.t += 12;
    }
}

/// Resume the driver where game code returned into it, with the registers
/// the game left. `None` if `pc` is not a resume point.
pub fn resume(pc: u16, cpu: &mut Z80, bus: &mut ColecoBus) -> Option<Flow> {
    let mut r = R::load(cpu);
    let flow = match pc {
        // PLAY_IT, after its first note: reassign and return.
        0x027b => {
            // CALL $0295; RET (the trap's).
            r.t += 17;
            reassign(&mut r, bus);
            Flow::Ret(r.t)
        }
        // SOUND_MAN's loop, after an area: the next area.
        0x028d => {
            r.t += 7 + 7 + 15 + 12;
            r.add_ix(10);
            return Some(sound_man_loop(cpu, bus, r));
        }
        // SOUND_MAN, after starting a note: the reassign check; its RET is
        // the trap's.
        0x02f5 => {
            area_tail(cpu, bus, &mut r);
            Flow::Ret(r.t)
        }
        // next_note's repeat, after PLAY_IT: return.
        0x039d => Flow::Ret(0),
        // A special note's first entry has returned: now its second. LD D,0,
        // LD E,7, ADD IY,DE, LD DE,nn, PUSH DE, JP (IY).
        0x03c6 => {
            r.t += 7 + 7 + 15 + 10 + 11 + 8;
            r.set_de(7);
            r.f = add16_flags(r.f, r.iy, 7);
            r.iy = r.iy.wrapping_add(7);
            r.set_de(0x0461);
            push(cpu, bus, 0x0461);
            r.store(cpu);
            cpu.pc = r.iy;
            return Some(Flow::Jump(r.t));
        }
        // A special note's second entry has returned: finish the note. Its
        // RET is counted in finish_note and made by the trap.
        0x0461 => {
            finish_note(cpu, bus, &mut r);
            Flow::Ret(r.t - 10)
        }
        _ => return None,
    };
    r.store(cpu);
    Some(flow)
}

/// A channel's attenuation onto the chip (`$0164`): `[4]`'s high nibble, or
/// its low nibble when C's bit 4 is clear (the noise control), with C's
/// command bits.
fn out_attenuation(r: &mut R, bus: &mut ColecoBus) {
    // CALL, LD A,(IX+4), BIT 4,C, JR Z, AND, OR C, OUT, RET; with the
    // rotation, JR Z not taken and four RRCA.
    r.t += 17 + 19 + 8 + 7 + 4 + 11 + 10;
    r.t += if r.c & 0x10 != 0 { 7 + 16 } else { 12 };
    r.a = peek(bus, r.ix.wrapping_add(4));
    r.f = bit_flags(r.f, 4, r.c);
    if r.c & 0x10 != 0 {
        for _ in 0..4 {
            r.rrca();
        }
    }
    r.and(0x0f);
    r.or(r.c);
    bus.output(0xff, r.a);
}

/// A tone channel's frequency onto the chip (`$0175`), in two bytes with D's
/// command bits on the first.
fn out_frequency(r: &mut R, bus: &mut ColecoBus) {
    // CALL; LD A,(IX+3), AND, OR D, OUT; LD A,(IX+3), AND, LD D,A,
    // LD A,(IX+4), AND, OR D, RRCA x4, OUT; RET.
    r.t += 17 + 19 + 7 + 4 + 11 + 19 + 7 + 4 + 19 + 7 + 4 + 16 + 11 + 10;
    let (lo, hi) = (peek(bus, r.ix.wrapping_add(3)), peek(bus, r.ix.wrapping_add(4)));
    r.a = lo;
    r.and(0x0f);
    r.or(r.d);
    bus.output(0xff, r.a);
    r.a = lo;
    r.and(0xf0);
    r.d = r.a;
    r.a = hi;
    r.and(0x0f);
    r.or(r.d);
    for _ in 0..4 {
        r.rrca();
    }
    bus.output(0xff, r.a);
}

/// PLAY_SONGS (`$1F61`), once a frame: each channel's area onto the chip.
/// Tones get attenuation and frequency; noise gets attenuation, and its
/// control only when it changed.
pub fn play_songs(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let mut r = R::load(cpu);
    let tones = [(0x9fu8, 0x90u8, 0x80u8, 0x7024u16), (0xbf, 0xb0, 0xa0, 0x7026), (0xdf, 0xd0, 0xc0, 0x7028)];
    for (silence, att, freq, ptr) in tones {
        // LD A,n, LD C,n, LD D,n, LD IX,(nn), CALL $034E; there LD E,(IX),
        // INC E, and JR NZ, then RET.
        r.t += 7 + 7 + 7 + 20 + 17 + 19 + 4 + 10;
        r.a = silence;
        r.c = att;
        r.d = freq;
        r.ix = ram16(bus, ptr);
        let v = peek(bus, r.ix);
        r.f = inc_flags(r.f, v);
        r.e = v.wrapping_add(1);
        if r.e == 0 {
            // JR NZ not taken, OUT, JR.
            r.t += 7 + 11 + 12;
            bus.output(0xff, r.a);
        } else {
            r.t += 12;
            out_attenuation(&mut r, bus);
            out_frequency(&mut r, bus);
        }
    }
    // LD A,n, LD C,n, LD IX,(nn), LD E,(IX), INC E.
    r.t += 7 + 7 + 20 + 19 + 4;
    r.a = 0xff;
    r.c = 0xf0;
    r.ix = ram16(bus, 0x7022);
    let v = peek(bus, r.ix);
    r.f = inc_flags(r.f, v);
    r.e = v.wrapping_add(1);
    if r.e == 0 {
        // JR NZ not taken, OUT, JR to the RET (the trap's).
        r.t += 7 + 11 + 12;
        bus.output(0xff, r.a);
    } else {
        // JR NZ, then after the attenuation LD A,(IX+4), AND, LD HL,nn,
        // CP (HL), and JR Z either way.
        r.t += 12;
        out_attenuation(&mut r, bus);
        r.t += 19 + 7 + 10 + 7;
        r.a = peek(bus, r.ix.wrapping_add(4));
        r.and(0x0f);
        r.set_hl(0x702a);
        let last = peek(bus, 0x702a);
        r.cp(last);
        if r.a != last {
            // JR Z not taken, LD (HL),A, LD C,n.
            r.t += 7 + 7 + 7;
            bus.write(0x702a, r.a);
            r.c = 0xe0;
            out_attenuation(&mut r, bus);
        } else {
            r.t += 12;
        }
    }
    r.store(cpu);
    r.t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One instruction on the real Z80 core from A, F and B: the flags and
    /// A (or B) it leaves. The oracle for the flag helpers.
    fn run(code: &[u8], a: u8, f: u8, b: u8) -> (u8, u8, u8) {
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
        cpu.step(&mut bus);
        (cpu.a(), cpu.f, (cpu.bc() >> 8) as u8)
    }

    #[test]
    fn flag_helpers_match_the_z80() {
        for v in 0..=255u8 {
            for f in [0x00u8, 0x01, 0xff] {
                let (_, zf, _) = run(&[0x04], 0, f, v); // INC B
                assert_eq!(inc_flags(f, v), zf, "INC {v:02X}");
                let (_, zf, _) = run(&[0x05], 0, f, v); // DEC B
                assert_eq!(dec_flags(f, v), zf, "DEC {v:02X}");
                let (za, zf, _) = run(&[0x0f], v, f, 0); // RRCA
                assert_eq!(rrca(v, f), (za, zf), "RRCA {v:02X}");
                let (_, zf, zb) = run(&[0xcb, 0x00], 0, f, v); // RLC B
                assert_eq!(rlc(v), (zb, zf), "RLC {v:02X}");
                for n in [0x00u8, 0x01, 0x0f, 0x7f, 0x80, 0xf0, 0xff] {
                    let (_, zf, _) = run(&[0x90], v, f, n); // SUB B
                    assert_eq!(sub_flags(v, n), zf, "SUB {v:02X},{n:02X}");
                    let (za, zf, _) = run(&[0x88], v, f, n); // ADC A,B
                    assert_eq!(add8(v, n, f & 1), (za, zf), "ADC {v:02X},{n:02X}");
                }
            }
        }
    }

    #[test]
    fn rlca_matches_the_z80() {
        for v in 0..=255u8 {
            for f in [0x00u8, 0xff] {
                let (za, zf, _) = run(&[0x07], v, f, 0);
                let mut r = R { a: v, f, b: 0, c: 0, d: 0, e: 0, h: 0, l: 0, ix: 0, iy: 0, t: 0 };
                r.rlca();
                assert_eq!((r.a, r.f), (za, zf), "RLCA {v:02X}");
            }
        }
    }
}
