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
//! **Special sounds call the cartridge.** A note of type 4, and every frame
//! of a song so marked, runs a routine in the game. Host code cannot call
//! guest code and wait, so at those points the driver does what the real one
//! does: it pushes the same values on the guest stack, jumps to the game's
//! routine, and resumes at a trap when the game returns, to the same
//! addresses the real driver resumes at. No state is kept on the host, so
//! save states need nothing new.

use super::routines::{ram16, Flow};
use crate::machine::ColecoBus;
use crate::z80::{Bus, Z80};

/// The idle-channel marker the channel pointers hold when nothing sounds.
pub const IDLE: u16 = 0x024c;

/// Where control comes back into the driver from game code: the continuation
/// traps. Each is where the real driver resumes.
pub const RESUME: [u16; 6] = [0x027b, 0x028d, 0x02f5, 0x039d, 0x03c6, 0x0461];

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

/// Song N's data area, and the address of its table entry.
fn song_area(bus: &mut ColecoBus, song: u8) -> (u16, u16) {
    let table = ram16(bus, 0x7020);
    // The real one scales the song number by rotating it left twice.
    let entry = table
        .wrapping_sub(4)
        .wrapping_add(song.rotate_left(2) as u16);
    (ram16(bus, entry.wrapping_add(2)), entry)
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
    for _ in 0..n {
        bus.write(area, 0xff);
        area = area.wrapping_add(10);
    }
    bus.write(area, 0);
    for ch in 0..4 {
        set16(bus, 0x7022 + 2 * ch, IDLE);
    }
    bus.write(0x702a, 0xff);
    // The idle marker is the last thing loaded into HL.
    cpu.set_hl(IDLE);
    cpu.set_de(10);
    cpu.set_bc(cpu.bc() & 0x00ff);
    120 + 40 * n as i32 + turn_off_sound(cpu, bus)
}

/// TURN_OFF_SOUND (`$1FD6`): all four channels to full attenuation.
pub fn turn_off_sound(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    for v in [0x9f, 0xbf, 0xdf, 0xff] {
        bus.output(0xff, v);
    }
    cpu.set_a(0xff);
    60
}

/// Point each channel at the last active area claiming it (`$0295`).
fn reassign(bus: &mut ColecoBus) {
    for ch in 0..4 {
        set16(bus, 0x7022 + 2 * ch, IDLE);
    }
    let (mut area, _) = song_area(bus, 1);
    loop {
        let v = bus.peek(area);
        if v == 0 {
            return;
        }
        if v != 0xff {
            let ch = (v >> 6) as u16;
            set16(bus, 0x7022 + 2 * ch, area);
        }
        area = area.wrapping_add(10);
    }
}

/// Decrement the low nibble of `(addr)` in place, the high nibble kept.
/// True when it has just reached zero.
fn dec_low_nibble(bus: &mut ColecoBus, addr: u16) -> bool {
    let v = bus.peek(addr);
    let low = (v & 0x0f).wrapping_sub(1) & 0x0f;
    bus.write(addr, (v & 0xf0) | low);
    low == 0
}

/// Reload the low nibble of `(addr)` from its high nibble.
fn reload_nibble(bus: &mut ColecoBus, addr: u16) {
    let hi = bus.peek(addr) & 0xf0;
    bus.write(addr, hi | (hi >> 4));
}

/// The volume sweep, once a frame (`$012F`). `[8]`: steps left (low nibble)
/// and the attenuation change (high nibble); `[9]`: frames per step, as a
/// counter (low) and its reload (high).
fn volume_sweep(bus: &mut ColecoBus, ix: u16) {
    if bus.peek(ix + 8) == 0 {
        return;
    }
    if !dec_low_nibble(bus, ix + 9) {
        return;
    }
    reload_nibble(bus, ix + 9);
    if dec_low_nibble(bus, ix + 8) {
        bus.write(ix + 8, 0);
        return;
    }
    let delta = bus.peek(ix + 8) & 0xf0;
    let v = bus.peek(ix + 4);
    bus.write(ix + 4, ((v & 0xf0).wrapping_add(delta)) & 0xf0 | (v & 0x0f));
}

/// Duration and the frequency sweep, once a frame (`$00FC`). False when the
/// note is over. With no sweep (`[7]` zero), `[5]` counts frames down. With
/// one, `[6]` counts frames per step, `[5]` counts steps, and each step adds
/// the signed `[7]` to the frequency as a 16-bit add over `[3..4]`: a borrow
/// runs into the attenuation nibble, as it does on the real BIOS.
fn duration_and_sweep(bus: &mut ColecoBus, ix: u16) -> bool {
    let step = bus.peek(ix + 7);
    if step == 0 {
        let d = bus.peek(ix + 5).wrapping_sub(1);
        if d == 0 {
            return false;
        }
        bus.write(ix + 5, d);
        return true;
    }
    if !dec_low_nibble(bus, ix + 6) {
        return true;
    }
    reload_nibble(bus, ix + 6);
    let steps = bus.peek(ix + 5).wrapping_sub(1);
    if steps == 0 {
        return false;
    }
    bus.write(ix + 5, steps);
    let freq = u16::from_le_bytes([bus.peek(ix + 3), bus.peek(ix + 4)]);
    let next = freq.wrapping_add(step as i8 as i16 as u16);
    let [lo, hi] = next.to_le_bytes();
    bus.write(ix + 3, lo);
    bus.write(ix + 4, hi & !0x04);
    true
}

/// What a step of the driver did: finished (the caller carries on, and the
/// trap returns to whoever called it), or handed control to game code with
/// the stack laid out for a resume.
enum Step {
    Done,
    Suspended,
}

/// Start the next note of the song in area `ix` (`$035F`). The caller has
/// pushed its return address, as the real CALL does; on `Done` it is still
/// there for the caller to take back.
fn next_note(cpu: &mut Z80, bus: &mut ColecoBus, ix: u16) -> Step {
    let song = bus.peek(ix) & 0x3f;
    // Pushed as AF, straight after the AND that isolated it.
    let song_f = super::routines::and_flags(song);
    push(cpu, bus, u16::from_be_bytes([song, song_f]));
    bus.write(ix, 0xff);
    let ptr = ram16(bus, ix + 1);
    let hdr = bus.peek(ptr);
    let note = |bus: &ColecoBus, i: u16| bus.peek(ptr.wrapping_add(i));
    if hdr & 0x20 != 0 {
        // A rest: silent for the low five bits' frames.
        push(cpu, bus, u16::from(hdr) << 8);
        set16(bus, ix + 1, ptr.wrapping_add(1));
        bus.write(ix + 4, 0xf0);
        bus.write(ix + 5, hdr & 0x1f);
        bus.write(ix + 7, 0);
        bus.write(ix + 8, 0);
    } else if hdr & 0x10 != 0 {
        if hdr & 0x08 != 0 {
            // Repeat: start the song again. The real driver pops the song
            // number it pushed and calls PLAY_IT, returning through $039D.
            pop(cpu, bus);
            push(cpu, bus, 0x039d);
            match play_it_song(cpu, bus, song) {
                Step::Suspended => return Step::Suspended,
                Step::Done => {
                    pop(cpu, bus);
                }
            }
            return Step::Done;
        }
        // The end of the song: the area stays idle.
        push(cpu, bus, 0xff00);
    } else if hdr & 0x3c == 0x04 {
        // A special: the note is a routine in the game. Run it, then its
        // second entry seven bytes on, then finish at $0461.
        push(cpu, bus, u16::from(hdr) << 8);
        let routine = u16::from_le_bytes([note(bus, 1), note(bus, 2)]);
        set16(bus, ix + 1, routine);
        // Entered as the real driver enters it: A and F as pushed with the
        // song number, DE the resume address, HL past the note.
        cpu.set_a(song);
        cpu.f = song_f;
        cpu.set_de(0x03c6);
        cpu.set_hl(ptr.wrapping_add(3));
        cpu.iy = routine;
        cpu.ix = ix;
        push(cpu, bus, 0x03c6);
        cpu.pc = routine;
        return Step::Suspended;
    } else {
        push(cpu, bus, u16::from(hdr) << 8);
        // Copy the note's bytes into the area's fields, last byte first as
        // the real driver's LDDR does; the note types differ in length.
        let (len, into): (u16, &[u16]) = match hdr & 0x03 {
            0 => (4, &[5, 4, 3]),
            1 => (6, &[7, 6, 5, 4, 3]),
            2 => (if hdr & 0xc0 == 0 { 5 } else { 6 }, &[9, 8, 5, 4, 3]),
            _ => (8, &[9, 8, 7, 6, 5, 4, 3]),
        };
        set16(bus, ix + 1, ptr.wrapping_add(len));
        let mut src = ptr.wrapping_add(len - 1);
        for &field in into {
            let v = bus.peek(src);
            bus.write(ix + field, v);
            src = src.wrapping_sub(1);
        }
        match hdr & 0x03 {
            0 => {
                bus.write(ix + 7, 0);
                bus.write(ix + 8, 0);
            }
            1 => bus.write(ix + 8, 0),
            2 => bus.write(ix + 7, 0),
            _ => {}
        }
    }
    finish_note(cpu, bus, ix);
    Step::Done
}

/// The end of next_note (`$0461`): mark the area with its channel and song,
/// or `$3E` for a special, unless the song ended. Pops the two values
/// next_note pushed.
fn finish_note(cpu: &mut Z80, bus: &mut ColecoBus, ix: u16) {
    let hdr = (pop(cpu, bus) >> 8) as u8;
    let song = (pop(cpu, bus) >> 8) as u8;
    cpu.set_hl(ix);
    if hdr == 0xff {
        return;
    }
    let id = if hdr & 0x3f == 0x04 { 0x3e } else { song };
    bus.write(ix, (hdr & 0xc0) | id);
}

/// PLAY_IT's body for song `song` (`$025E`): unless that song is already
/// playing, point its area at the start of its note list, start the first
/// note, and reassign the channels.
fn play_it_song(cpu: &mut Z80, bus: &mut ColecoBus, song: u8) -> Step {
    let (ix, entry) = song_area(bus, song);
    if bus.peek(ix) & 0x3f == song {
        return Step::Done;
    }
    bus.write(ix, song);
    let notes = ram16(bus, entry);
    set16(bus, ix + 1, notes);
    push(cpu, bus, 0x027b);
    if let Step::Suspended = next_note(cpu, bus, ix) {
        return Step::Suspended;
    }
    pop(cpu, bus);
    reassign(bus);
    Step::Done
}

/// PLAY_IT (`$1FF1`): start song B.
pub fn play_it(cpu: &mut Z80, bus: &mut ColecoBus) -> Flow {
    let song = (cpu.bc() >> 8) as u8;
    match play_it_song(cpu, bus, song) {
        Step::Done => Flow::Ret(250),
        Step::Suspended => Flow::Jump(250),
    }
}

/// SOUND_MAN (`$1FF4`), once a frame: every active area's sweeps and
/// duration, and the next note when one ends.
pub fn sound_man(cpu: &mut Z80, bus: &mut ColecoBus) -> Flow {
    let (ix, _) = song_area(bus, 1);
    sound_man_from(cpu, bus, ix)
}

fn sound_man_from(cpu: &mut Z80, bus: &mut ColecoBus, mut ix: u16) -> Flow {
    let mut cycles = 60;
    loop {
        let v = bus.peek(ix);
        if v == 0 {
            cpu.ix = ix;
            return Flow::Ret(cycles);
        }
        cycles += 120;
        if v != 0xff {
            if v & 0x3f == 0x3e {
                // A special's frame: its routine's second entry, returning
                // to the loop at $028D.
                push(cpu, bus, 0x028d);
                cpu.ix = ix;
                cpu.pc = ram16(bus, ix + 1).wrapping_add(7);
                return Flow::Jump(cycles);
            }
            volume_sweep(bus, ix);
            if !duration_and_sweep(bus, ix) {
                push(cpu, bus, 0x028d);
                push(cpu, bus, u16::from(v) << 8);
                push(cpu, bus, 0x02f5);
                cpu.ix = ix;
                if let Step::Suspended = next_note(cpu, bus, ix) {
                    return Flow::Jump(cycles);
                }
                pop(cpu, bus);
                area_tail(cpu, bus, ix);
                pop(cpu, bus);
            }
        }
        ix = ix.wrapping_add(10);
    }
}

/// After a note change in SOUND_MAN (`$02F5`): if the area's song or
/// channel changed, reassign. Pops the old first byte SOUND_MAN pushed.
fn area_tail(cpu: &mut Z80, bus: &mut ColecoBus, ix: u16) {
    let old = (pop(cpu, bus) >> 8) as u8;
    if bus.peek(ix) != old {
        reassign(bus);
    }
}

/// Resume the driver where game code returned into it. `None` if `pc` is
/// not a resume point.
pub fn resume(pc: u16, cpu: &mut Z80, bus: &mut ColecoBus) -> Option<Flow> {
    Some(match pc {
        // PLAY_IT, after its first note: reassign and return.
        0x027b => {
            reassign(bus);
            Flow::Ret(80)
        }
        // SOUND_MAN's loop, after an area: the next area.
        0x028d => {
            let ix = cpu.ix.wrapping_add(10);
            sound_man_from(cpu, bus, ix)
        }
        // SOUND_MAN, after starting a note: the reassign check.
        0x02f5 => {
            let ix = cpu.ix;
            area_tail(cpu, bus, ix);
            Flow::Ret(40)
        }
        // next_note's repeat, after PLAY_IT: return.
        0x039d => Flow::Ret(10),
        // A special note's first entry has returned: now its second.
        0x03c6 => {
            cpu.set_de(7);
            cpu.iy = cpu.iy.wrapping_add(7);
            push(cpu, bus, 0x0461);
            cpu.pc = cpu.iy;
            Flow::Jump(40)
        }
        // A special note's second entry has returned: finish the note.
        0x0461 => {
            let ix = cpu.ix;
            finish_note(cpu, bus, ix);
            Flow::Ret(40)
        }
        _ => return None,
    })
}

/// PLAY_SONGS (`$1F61`), once a frame: each channel's area onto the chip.
/// Tones get attenuation and frequency; noise gets attenuation, and its
/// control only when it changed.
pub fn play_songs(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    for (ptr, silence, att, freq) in [
        (0x7024u16, 0x9fu8, 0x90u8, 0x80u8),
        (0x7026, 0xbf, 0xb0, 0xa0),
        (0x7028, 0xdf, 0xd0, 0xc0),
    ] {
        let area = ram16(bus, ptr);
        if bus.peek(area) == 0xff {
            bus.output(0xff, silence);
            continue;
        }
        let (f_lo, f_hi) = (bus.peek(area + 3), bus.peek(area + 4));
        bus.output(0xff, (f_hi >> 4) | att);
        bus.output(0xff, (f_lo & 0x0f) | freq);
        bus.output(0xff, (f_lo >> 4) | ((f_hi & 0x0f) << 4));
    }
    let area = ram16(bus, 0x7022);
    if bus.peek(area) == 0xff {
        bus.output(0xff, 0xff);
    } else {
        let v = bus.peek(area + 4);
        bus.output(0xff, (v >> 4) | 0xf0);
        let ctrl = v & 0x0f;
        if ctrl != bus.peek(0x702a) {
            bus.write(0x702a, ctrl);
            bus.output(0xff, ctrl | 0xe0);
        }
    }
    cpu.ix = ram16(bus, 0x7022);
    400
}
