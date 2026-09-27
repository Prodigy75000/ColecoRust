// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The HLE BIOS: the point of this core.
//!
//! Designed from the census (`docs/notes/CENSUS-2026-09-27.md`) and the
//! hand-over measurement (`handover` harness), both taken on the real BIOS as
//! the oracle. The census settled the shape: games both CALL the BIOS and READ
//! it, so the HLE is two things at once:
//!
//! - **An 8 KB image** ([`image`]) holding what games read or jump through:
//!   the RST and NMI vectors, which pass straight on to the cartridge; the
//!   refresh-rate byte and font pointers at `$0069`-`$006D`; our own font at
//!   `$158B` ([`font`]); and the jump table at `$1F61`-`$1FFD`, whose slots
//!   jump to the same routine addresses the real BIOS uses. Those addresses
//!   are interface, not code: games call some of them directly, bypassing the
//!   table, so they have to be where the real ones are.
//! - **Traps** ([`trap`]): when the PC reaches a routine address, the machine
//!   runs host code instead of the byte there. A routine not written yet is
//!   logged and returns at once, so a game degrades rather than crashes, and
//!   the log says what to write next.
//!
//! **Boot is skipped, by the owner's decision (2026-09-27).** The reset trap
//! builds the state the real BIOS leaves at hand-over and jumps to the game.
//! No title screen, no delay.
//!
//! Nothing here keeps state between calls yet. When a routine needs some, it
//! goes where the real BIOS keeps it in guest RAM, which the census located,
//! so the save-state layout does not change.

pub mod font;
pub mod routines;
pub mod sound;

use crate::machine::ColecoBus;
use crate::z80::{Bus, Z80};
use crate::BIOS_SIZE;
use std::collections::BTreeMap;

/// The jump table: each slot and the routine address it jumps to, as the real
/// BIOS has them. Addresses only, read off the real BIOS: they are the
/// interface games were written against.
pub const TABLE: [(u16, u16); 53] = [
    (0x1f61, 0x0300), (0x1f64, 0x0488), (0x1f67, 0x06c7), (0x1f6a, 0x1d5a), (0x1f6d, 0x1d60),
    (0x1f70, 0x1d66), (0x1f73, 0x1d6c), (0x1f76, 0x114a), (0x1f79, 0x118b), (0x1f7c, 0x1979),
    (0x1f7f, 0x1927), (0x1f82, 0x18d4), (0x1f85, 0x18e9), (0x1f88, 0x116a), (0x1f8b, 0x1b0e),
    (0x1f8e, 0x1b8c), (0x1f91, 0x1c10), (0x1f94, 0x1c5a), (0x1f97, 0x1c76), (0x1f9a, 0x0f9a),
    (0x1f9d, 0x0fb8), (0x1fa0, 0x1044), (0x1fa3, 0x10bf), (0x1fa6, 0x1cbc), (0x1fa9, 0x1ced),
    (0x1fac, 0x1d2a), (0x1faf, 0x0655), (0x1fb2, 0x0203), (0x1fb5, 0x0251), (0x1fb8, 0x1b1d),
    (0x1fbb, 0x1ba3), (0x1fbe, 0x1c27), (0x1fc1, 0x1c66), (0x1fc4, 0x1c82), (0x1fc7, 0x0faa),
    (0x1fca, 0x0fc4), (0x1fcd, 0x1053), (0x1fd0, 0x10cb), (0x1fd3, 0x0f37), (0x1fd6, 0x023b),
    (0x1fd9, 0x1cca), (0x1fdc, 0x1d57), (0x1fdf, 0x1d01), (0x1fe2, 0x1d3e), (0x1fe5, 0x0664),
    (0x1fe8, 0x0679), (0x1feb, 0x11c1), (0x1fee, 0x0213), (0x1ff1, 0x025e), (0x1ff4, 0x027f),
    (0x1ff7, 0x04a3), (0x1ffa, 0x06d8), (0x1ffd, 0x003b),
];

/// GAME_OPT's text, at the addresses the real routine reads it from: two
/// headings, the option line it writes eight times, and the pieces patched
/// into it for the other seven. Plain instructions to the player, and the
/// whole point of the screen, so the words are the real ones.
pub const GAME_OPT_TEXT: (u16, &[u8]) =
    (0x1a7c, b"TO SELECT GAME OPTION,PRESS BUTTON ON KEYPAD.1 = SKILL 1/ONE PLAYER2345678TWOS");

/// Where the reset trap parks a machine whose cartridge has no valid header:
/// a HALT with interrupts off. The real BIOS shows a "turn game off" screen
/// there; a blank screen says the same thing.
const PARK: u16 = 0x0003;

/// The stack pointer the BIOS hands over with.
const HANDOVER_SP: u16 = 0x73b9;

/// Cycles charged for the reset trap. The real boot is skipped, so this is
/// nominal.
const BOOT_CYCLES: i32 = 100;

/// Build the 8 KB image.
///
/// Unused space is `$00`, not `$FF`. It is never executed (a jump into it is
/// a wild entry, trapped), but it IS read: games copy BIOS graphics they do
/// not get from us (the title-screen logo tiles at `$14C3`) and read odd
/// bytes as constants (Facemaker reads `$0100`, a zero on the real BIOS).
/// Zero draws as empty tiles; `$FF` drew solid blocks.
pub fn image() -> Box<[u8; BIOS_SIZE]> {
    let mut b = Box::new([0x00u8; BIOS_SIZE]);
    // Reset: never executed (the PC-0 trap boots), but shaped like a boot so a
    // reader of the image sees what it stands for. $0003 is the parking HALT.
    b[0x0000..0x0004].copy_from_slice(&[0x31, 0x73b9u16 as u8, (0x73b9u16 >> 8) as u8, 0x76]);
    // RST 08-38 pass on to the cartridge's vectors at $800C, $800F ... $801E.
    for k in 0..7usize {
        let at = 0x08 + k * 8;
        let to = 0x800c + k as u16 * 3;
        b[at..at + 3].copy_from_slice(&[0xc3, to as u8, (to >> 8) as u8]);
    }
    // NMI passes on to the cartridge's vector at $8021.
    b[0x0066..0x0069].copy_from_slice(&[0xc3, 0x21, 0x80]);
    // 60 Hz, and the pointers to 'A' and '0' in the font.
    b[0x0069] = 60;
    b[0x006a..0x006c].copy_from_slice(&0x16abu16.to_le_bytes());
    b[0x006c..0x006e].copy_from_slice(&0x1623u16.to_le_bytes());
    for c in font::FIRST..=0x7f {
        let at = font::BASE as usize + (c - font::FIRST) as usize * 8;
        b[at..at + 8].copy_from_slice(&font::pattern(c));
    }
    let (at, text) = GAME_OPT_TEXT;
    b[at as usize..at as usize + text.len()].copy_from_slice(text);
    // The sound driver's idle-channel marker: channels with nothing to play
    // point here, and the driver reads its first byte as "idle".
    b[sound::IDLE as usize] = 0xff;
    for &(slot, target) in &TABLE {
        let s = slot as usize;
        b[s..s + 3].copy_from_slice(&[0xc3, target as u8, (target >> 8) as u8]);
        // A RET where the routine lives. Never executed while the trap is in
        // place; it keeps a stray jump from running into the font.
        b[target as usize] = 0xc9;
    }
    b
}

/// What the HLE saw that it could not serve. Diagnostics, not machine state.
#[derive(Default, Debug, Clone)]
pub struct HleLog {
    /// Routine address to calls, for routines not written yet.
    pub unimplemented: BTreeMap<u16, u64>,
    /// BIOS addresses reached that are neither code in the image nor a
    /// routine: a game jumping into the middle of something.
    pub wild: BTreeMap<u16, u64>,
    /// For each wild address, the PC of the instruction that led there.
    pub wild_from: BTreeMap<u16, u16>,
    /// PC of the last instruction executed, kept by the machine.
    pub prev_pc: u16,
}

/// True for addresses where the image holds real code to execute: the parking
/// HALT, the RST and NMI vectors, and the jump table slots.
fn runs_from_image(pc: u16) -> bool {
    pc == PARK
        || (0x0008..0x003b).contains(&pc) && (pc & 7) < 3
        || (0x0066..0x0069).contains(&pc)
        || (0x1f61..0x2000).contains(&pc)
}

/// Called before each instruction with the PC in the BIOS window. `None`: the
/// CPU executes the image normally. `Some(cycles)`: the HLE handled it.
pub fn trap(cpu: &mut Z80, bus: &mut ColecoBus, log: &mut HleLog) -> Option<i32> {
    let pc = cpu.pc;
    if pc == 0 {
        boot(cpu, bus);
        return Some(BOOT_CYCLES);
    }
    if runs_from_image(pc) {
        return None;
    }
    // Game code returning into the sound driver after a special sound.
    let flow = match sound::resume(pc, cpu, bus) {
        Some(flow) => Some(flow),
        None if TABLE.iter().any(|&(_, t)| t == pc) => routines::call(pc, cpu, bus),
        None => None,
    };
    match flow {
        Some(routines::Flow::Ret(c)) => return Some(c + ret(cpu, bus)),
        Some(routines::Flow::Jump(c)) => return Some(c),
        None => {}
    }
    let known = TABLE.iter().any(|&(_, t)| t == pc);
    // Not written yet, or not a routine at all: logged, and it returns.
    if known {
        *log.unimplemented.entry(pc).or_default() += 1;
    } else {
        *log.wild.entry(pc).or_default() += 1;
        log.wild_from.entry(pc).or_insert(log.prev_pc);
    }
    Some(ret(cpu, bus))
}

/// Return to the caller, as a RET would.
fn ret(cpu: &mut Z80, bus: &mut ColecoBus) -> i32 {
    let lo = bus.read(cpu.sp);
    let hi = bus.read(cpu.sp.wrapping_add(1));
    cpu.sp = cpu.sp.wrapping_add(2);
    cpu.pc = u16::from_le_bytes([lo, hi]);
    10
}

/// The hand-over, as the real BIOS leaves the machine at the game's first
/// instruction. Measured with the `handover` harness across every title
/// (2026-09-27), split by the header's first two bytes.
fn boot(cpu: &mut Z80, bus: &mut ColecoBus) {
    let header = [bus.peek(0x8000), bus.peek(0x8001)];
    let start = u16::from_le_bytes([bus.peek(0x800a), bus.peek(0x800b)]);
    cpu.sp = HANDOVER_SP;
    cpu.iff1 = false;
    cpu.iff2 = false;
    cpu.im = 0;
    match header {
        // "Skip the title screen": the real BIOS touches nothing else. VDP,
        // VRAM and RAM stay as power-on left them, and the game sets them up.
        [0x55, 0xaa] => {
            cpu.set_a(0xaa);
            cpu.f = 0x6a;
        }
        // "Show the title screen": skipped by decision, but the game is handed
        // the machine the title screen would have left.
        [0xaa, 0x55] => {
            cpu.set_a(0x80);
            cpu.f = 0x42;
            cpu.set_bc(0x0180);
            cpu.set_de(0x0000);
            cpu.ix = 0x73f6;
            cpu.iy = 0x1c01;
            // Display off, 16K, and the table layout the title screen used.
            bus.vdp.regs = [0x00, 0x80, 0x06, 0x80, 0x00, 0x36, 0x07, 0x00];
            // The uppercase character set at character x 8, white on
            // transparent, as a game that prints without loading a font of
            // its own expects to find it. Ours, not the BIOS's glyphs.
            for c in font::FIRST..0x60 {
                let at = c as usize * 8;
                bus.vdp.vram[at..at + 8].copy_from_slice(&font::pattern(c));
            }
            for group in 3..=11 {
                bus.vdp.vram[0x2000 + group] = 0xf0;
            }
            // RAM identical across every such title at hand-over; the two
            // stack bytes that vary with the title's text take their most
            // common values ($73AF=09, $73B3=3F).
            for &(addr, v) in &[
                (0x73adu16, 0x08u8), (0x73af, 0x09), (0x73b1, 0x04), (0x73b3, 0x3f), (0x73b4, 0x80),
                (0x73b5, 0x55), (0x73b6, 0x1c), (0x73b7, 0xfb), (0x73b8, 0x13), (0x73c4, 0x80),
                (0x73c8, 0x33), (0x73f3, 0x1b), (0x73f5, 0x38), (0x73f7, 0x18), (0x73fb, 0x20),
                (0x73fe, 0x04),
            ] {
                bus.write(addr, v);
            }
        }
        _ => {
            cpu.pc = PARK;
            return;
        }
    }
    cpu.set_hl(start);
    cpu.pc = start;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_slot_jumps_to_its_routine_and_the_vectors_pass_through() {
        let b = image();
        for &(slot, target) in &TABLE {
            let s = slot as usize;
            assert_eq!(&b[s..s + 3], &[0xc3, target as u8, (target >> 8) as u8]);
        }
        assert_eq!(&b[0x08..0x0b], &[0xc3, 0x0c, 0x80], "RST 08");
        assert_eq!(&b[0x38..0x3b], &[0xc3, 0x1e, 0x80], "RST 38");
        assert_eq!(&b[0x66..0x69], &[0xc3, 0x21, 0x80], "NMI");
        assert_eq!(b[0x69], 60);
    }

    /// No routine address lands inside the font, and no two slots collide:
    /// the image's pieces do not overwrite each other.
    #[test]
    fn the_pieces_of_the_image_do_not_overlap() {
        let font = font::BASE..=0x18a2;
        let (at, text) = GAME_OPT_TEXT;
        let opt = at..at + text.len() as u16;
        for &(_, t) in &TABLE {
            assert!(!font.contains(&t), "routine {t:04X} inside the font");
            assert!(!opt.contains(&t), "routine {t:04X} inside GAME_OPT's text");
            assert!(!runs_from_image(t), "routine {t:04X} would execute from the image");
        }
    }

    /// Code the image must execute is not mistaken for a routine to trap.
    #[test]
    fn vectors_run_from_the_image() {
        for pc in [0x0003, 0x0008, 0x0009, 0x000a, 0x0038, 0x003a, 0x0066, 0x1f61, 0x1ffd] {
            assert!(runs_from_image(pc), "{pc:04X}");
        }
        for pc in [0x000b, 0x003b, 0x0069, 0x0300] {
            assert!(!runs_from_image(pc), "{pc:04X}");
        }
    }
}
