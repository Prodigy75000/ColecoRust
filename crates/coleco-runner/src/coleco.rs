// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! `coleco`: the first dev harness. Boot something and step it.
//!
//! Nothing to boot yet. This exists so the workspace has a binary that builds
//! and runs from day one: the first thing a new core needs is a way to say
//! "here is the machine, here is what it did", and retrofitting that once there
//! is a CPU to debug is how a project ends up debugging through `println!`.

use coleco_core::{BiosMode, BIOS_SIZE, CPU_HZ, SAVE_STATE_VERSION, VRAM, WORK_RAM};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("coleco: ColecoRust dev harness");
        println!();
        println!("usage: coleco [--help]");
        println!();
        println!("Nothing is implemented yet. The objective is the HLE BIOS:");
        println!("a cartridge and nothing else, on the majority of titles.");
        return;
    }

    println!("ColecoRust {}", env!("CARGO_PKG_VERSION"));
    println!("save-state format: v{SAVE_STATE_VERSION}");
    println!("default BIOS mode : {:?}", BiosMode::default());
    println!();
    println!("  Z80A        {:>9} Hz", CPU_HZ);
    println!("  work RAM    {:>9} bytes", WORK_RAM);
    println!("  VDP VRAM    {:>9} bytes", VRAM);
    println!("  BIOS window {:>9} bytes at 0x0000", BIOS_SIZE);
    println!();
    println!("No CPU, no VDP, no PSG, no HLE. Nothing to run.");
}
