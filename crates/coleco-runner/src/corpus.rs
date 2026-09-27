// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! What `smoke` and `census` share, so the two drive every cartridge the same
//! way and their ledgers line up row for row: the directory walk, the title
//! classification, and the input script.
#![allow(dead_code)]

use coleco_core::machine::Pad;
use std::path::{Path, PathBuf};

/// Frames each cartridge runs: 25 s.
pub const FRAMES: u32 = 1500;
/// Keypad 1 held for ten frames at each of these (the first just after the
/// title screen hands over at about frame 670; "1" is the usual "skill 1,
/// one player"). One press was not enough: a game that reaches its menu
/// late, or wants a key released first, never saw it.
const KEY_AT: [u32; 3] = [760, 1000, 1240];
/// Both fire buttons for ten frames at each of these.
const FIRE_AT: [u32; 2] = [880, 1120];
const HOLD: u32 = 10;
pub const FIRST_INPUT: u32 = KEY_AT[0];

/// Both controllers on frame `f`. Keys and fire go to BOTH: Destructor and
/// Turbo, the two Driving Module titles, ignored controller 1's keypad in the
/// first runs; the wheel has no keypad, so they read port 2's.
pub fn pads_at(f: u32) -> [Pad; 2] {
    let held = |at: &[u32]| at.iter().any(|&a| (a..a + HOLD).contains(&f));
    let key = held(&KEY_AT).then_some(1);
    let fire = held(&FIRE_AT);
    let pad = Pad { key, fire_left: fire, fire_right: fire, ..Pad::default() };
    [pad, pad]
}

/// Every cartridge under `dir`, sorted, minus the BIOS and its hacks, which
/// the collection also ships as .col files.
pub fn cartridges(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    collect(dir, &mut v);
    v.retain(|p| !p.to_string_lossy().contains("ColecoVision BIOS"));
    v
}

pub fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("col")) {
            out.push(p);
        }
    }
}

/// Commercial or public domain, the dump's GoodTools-style status, and the
/// title with every parenthesised and bracketed tag removed, which is what
/// variants of one game share. The denominator is counted in titles, not
/// files: the collection's 300 cartridge files are far fewer games.
pub fn classify(file: &str) -> (&'static str, &'static str, String) {
    let class = if file.contains("(PD)") || file.contains("Public Domain/") { "pd" } else { "commercial" };
    let name = file.rsplit('/').next().unwrap_or(file);
    let dump = if name.contains("[b") {
        "bad"
    } else if name.contains("[h") {
        "hack"
    } else if name.contains("[t") {
        "trainer"
    } else if name.contains("[a") {
        "alt"
    } else {
        "good"
    };
    let stem = name.trim_end_matches(".col");
    let cut = stem.find(['(', '[']).unwrap_or(stem.len());
    (class, dump, stem[..cut].trim().to_string())
}

/// What the first two bytes say the BIOS should do with the cartridge.
pub fn header(cart: &[u8]) -> &'static str {
    match cart.get(0..2) {
        Some([0xaa, 0x55]) => "title",
        Some([0x55, 0xaa]) => "skip",
        _ => "none",
    }
}

