// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! ColecoRust: the machine.
//!
//! A clean-room ColecoVision core. Nothing is implemented yet; this file holds
//! the constraints and the shape so they are in front of whoever writes the
//! first module rather than in a chat log.
//!
//! # The objective is the HLE BIOS
//!
//! The rest of this machine is small and well documented. What the project is
//! FOR is that a user brings a cartridge and nothing else, on the majority of
//! commercial titles, with no BIOS dump present.
//!
//! That is not "skip the logo". Games keep calling into the BIOS after boot:
//! the controller decoder lives there and the keypad is read through it, the
//! sound driver lives there and games hand it data structures rather than
//! writing the [`Sn76489`] directly, and games depend on RAM the BIOS
//! maintains, so an HLE has to leave the same bytes in the same places. It is a
//! reimplementation of a published interface, judged against a corpus.
//!
//! A real BIOS dump is an ORACLE, never a requirement. Run a title both ways
//! and diff; when they disagree, the real one is right until proven otherwise.
//!
//! # Binding contracts inherited from the other in-house cores
//!
//! - **Save states are byte-identical across architectures.** Fixed size,
//!   little-endian, versioned. Same bytes on x86-64 and arm64, because netplay
//!   between a PC and a phone rests on it. Rules out serialising anything
//!   host-shaped: pointers, `usize`, hash iteration order, enum discriminants a
//!   compiler may renumber.
//! - **A libretro core must expose `SET_MEMORY_MAPS` and `SYSTEM_RAM`**, or
//!   RetroAchievements hangs on "waiting for core memory map".
//!
//! # One number worth knowing before designing anything
//!
//! This machine has **1 KB of work RAM**. Not 1 MB. Every byte the HLE BIOS
//! leaves behind competes with the game for it, so the HLE cannot keep its
//! bookkeeping in guest RAM wherever it likes: it has to put exactly what the
//! real BIOS puts, where the real BIOS puts it, and keep its own state on the
//! host side.

pub mod census;
pub mod hle;
pub mod machine;
pub mod psg;
pub mod save;
pub mod vdp;
pub mod z80;

/// Z80A clock, 3.579545 MHz. One third of the NTSC colour burst, which is why
/// it is that number and not a round one.
pub const CPU_HZ: u32 = 3_579_545;

/// Work RAM, in bytes. Yes, one kilobyte.
pub const WORK_RAM: usize = 1024;

/// The TMS9918A has its own 16 KB of VRAM, reached through the VDP ports rather
/// than mapped into the Z80 address space.
pub const VRAM: usize = 16 * 1024;

/// The BIOS occupies the bottom 8 KB of the address space.
///
/// Under HLE nothing is loaded here, but the ADDRESSES still matter: games call
/// documented entry points in this range, and that is the surface the HLE has
/// to present.
pub const BIOS_BASE: u16 = 0x0000;
pub const BIOS_SIZE: usize = 8 * 1024;

/// Save-state format version.
///
/// Bump on ANY layout change. A state written by a newer build must be refused
/// by an older one rather than misread.
///
/// v1: the first layout (Z80, RAM, VDP, PSG and resampler, controller mode,
/// the NMI line, the line cycle counter), 2026-09-27.
/// v2: whether a scanline is open, so a state can be taken mid-line.
pub const SAVE_STATE_VERSION: u32 = 2;

/// How the machine gets its BIOS behaviour.
///
/// The default is [`BiosMode::Hle`] on purpose: a firmware dump is a
/// development oracle, and treating it as the normal path is how a project ends
/// up shipping something that needs one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BiosMode {
    /// Reimplemented entry points. No dump present, and none needed.
    #[default]
    Hle,
    /// A real dump, for differential testing against the HLE.
    Real,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hle_is_the_default_path_rather_than_the_fallback() {
        // If this ever flips, the project has quietly become one that needs a
        // firmware file, which is the single thing it exists to avoid.
        assert_eq!(BiosMode::default(), BiosMode::Hle);
    }

    #[test]
    fn the_machine_constants_are_the_real_ones() {
        // 3.579545 MHz is one third of the NTSC colour burst. A round 3.5 MHz
        // here would be wrong in a way that looks plausible in a log.
        assert_eq!(CPU_HZ, 3_579_545);
        // One kilobyte. Guarded because it is the number people assume is a typo
        // and "fix".
        assert_eq!(WORK_RAM, 1024);
        assert_eq!(VRAM, 16_384);
        assert_eq!(BIOS_SIZE, 8_192);
    }
}
