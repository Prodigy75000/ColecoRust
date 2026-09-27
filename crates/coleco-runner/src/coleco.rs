// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! `coleco`: boot a cartridge, run it, and say what it did.
//!
//!   coleco --bios bios/coleco.rom --frames 120 --png out/dk.png dumps/.../Donkey Kong.col
//!
//! With no `--bios` the machine is asked for the HLE, which does not exist
//! yet and says so. Prints where the CPU ended up and how many NMIs it took,
//! and optionally writes the last frame as a PNG.
//!
//! `--key FRAME:K` holds keypad key K (0-9, `*`, `#`) on controller 1 for ten
//! frames from FRAME, and can be given more than once. `--fire FRAME` holds
//! both fire buttons for ten frames the same way. `--until-cart` stops at the
//! cartridge's first instruction instead, and `--vram OUT` writes the 16 KB of
//! VRAM as it stands at the end. `--probe` attaches the census probe and
//! prints the BIOS entries it saw, with their callers, and the data reads.
//! `--state FILE` starts from a save state (a device's `.state` is one) after
//! building the machine on the same cartridge and BIOS mode. `--save-state
//! FILE` writes one at the end, so a real-BIOS run stopped with `--until-cart`
//! can be continued on the HLE from the very same machine. `--trace-vdp`
//! prints every change to a VDP register as it happens, with the frame, the
//! instruction's address and whether an NMI handler was running.

use coleco_core::machine::{Coleco, Firmware, Pad};
use coleco_core::vdp::{HEIGHT, WIDTH};
use coleco_core::{BiosMode, CPU_HZ, SAVE_STATE_VERSION};

#[path = "png.rs"]
mod png;
use png::write_png;

fn usage() -> ! {
    eprintln!("usage: coleco [--bios PATH] [--frames N] [--png OUT] [--key FRAME:K]... [--fire FRAME]... [--until-cart] [--vram OUT] [--probe] [--state FILE] [--save-state FILE] [--trace-vdp] CART");
    eprintln!();
    eprintln!("Runs CART for N frames (default 60) on the real BIOS at PATH, or on");
    eprintln!("the HLE when no --bios is given, and optionally writes the last frame.");
    std::process::exit(2);
}

fn main() {
    let mut bios: Option<String> = None;
    let mut frames: u32 = 60;
    let mut png: Option<String> = None;
    let mut cart: Option<String> = None;
    let mut keys: Vec<(u32, u8)> = Vec::new();
    let mut fires: Vec<u32> = Vec::new();
    let mut until_cart = false;
    let mut probe = false;
    let mut state: Option<String> = None;
    let mut vram_out: Option<String> = None;
    let mut save_to: Option<String> = None;
    let mut trace_vdp = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bios" => bios = Some(args.next().unwrap_or_else(|| usage())),
            "--frames" => frames = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--png" => png = Some(args.next().unwrap_or_else(|| usage())),
            "--until-cart" => until_cart = true,
            "--probe" => probe = true,
            "--state" => state = Some(args.next().unwrap_or_else(|| usage())),
            "--trace-vdp" => trace_vdp = true,
            "--save-state" => save_to = Some(args.next().unwrap_or_else(|| usage())),
            "--vram" => vram_out = Some(args.next().unwrap_or_else(|| usage())),
            "--key" => keys.push(parse_key(&args.next().unwrap_or_else(|| usage()))),
            "--fire" => fires.push(args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            _ if cart.is_none() => cart = Some(a),
            _ => usage(),
        }
    }
    let Some(cart_path) = cart else { usage() };
    let cart = std::fs::read(&cart_path).unwrap_or_else(|e| {
        eprintln!("{cart_path}: {e}");
        std::process::exit(1)
    });

    let (mode, firmware) = match &bios {
        None => (BiosMode::Hle, Firmware::Hle),
        Some(p) => {
            let b = std::fs::read(p).unwrap_or_else(|e| {
                eprintln!("{p}: {e}");
                std::process::exit(1)
            });
            (BiosMode::Real, Coleco::bios_from_bytes(&b).unwrap_or_else(|e| {
                eprintln!("{p}: {e:?}");
                std::process::exit(1)
            }))
        }
    };
    let mut m = Coleco::new(firmware, &cart).unwrap_or_else(|e| {
        eprintln!("cannot start in {mode:?} mode: {e:?}");
        std::process::exit(1)
    });

    if let Some(p) = &state {
        let bytes = std::fs::read(p).unwrap_or_else(|e| {
            eprintln!("{p}: {e}");
            std::process::exit(1)
        });
        if let Err(e) = m.load_state(&bytes) {
            eprintln!("{p}: cannot load: {e:?}");
            std::process::exit(1);
        }
    }
    if probe {
        m.bus.probe = Some(Box::new(coleco_core::census::Probe::new()));
    }
    if until_cart {
        let mut steps = 0u64;
        while m.cpu.pc < 0x8000 && steps < 40_000_000 {
            m.step();
            steps += 1;
        }
        frames = 0;
    }
    for f in 0..frames {
        let key = keys.iter().find(|&&(at, _)| f >= at && f < at + 10).map(|&(_, k)| k);
        let fire = fires.iter().any(|&at| f >= at && f < at + 10);
        m.bus.pads[0] = Pad { key, fire_left: fire, fire_right: fire, ..Pad::default() };
        if !trace_vdp {
            m.run_frame();
            continue;
        }
        // Step to the frame's end, watching the registers.
        loop {
            let (before, pc, nmis) = (m.bus.vdp.regs, m.cpu.pc, m.nmis);
            m.step();
            if m.bus.vdp.regs != before {
                for r in 0..8 {
                    if m.bus.vdp.regs[r] != before[r] {
                        let nmi = if m.nmis != nmis { " (NMI taken)" } else { "" };
                        println!("  vdp      frame {f} R{r} {:02X} -> {:02X} at {pc:04X}{nmi}", before[r], m.bus.vdp.regs[r]);
                    }
                }
            }
            if !m.in_line() && m.bus.vdp.line == 0 {
                break;
            }
        }
    }
    let audio = m.take_audio();
    let peak = audio.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);

    println!("ColecoRust, save-state v{SAVE_STATE_VERSION}, {mode:?} BIOS");
    println!("  cart     {cart_path} ({} bytes)", cart.len());
    println!("  frames   {frames} ({:.2} s emulated at {CPU_HZ} Hz)", frames as f64 * 262.0 * 228.0 / CPU_HZ as f64);
    println!("  nmis     {}", m.nmis);
    println!(
        "  cpu      PC {:04X} SP {:04X} AF {:02X}{:02X} BC {:04X} DE {:04X} HL {:04X} IM {} IFF1 {}",
        m.cpu.pc, m.cpu.sp, m.cpu.a(), m.cpu.f, m.cpu.bc(), m.cpu.de(), m.cpu.hl(), m.cpu.im, m.cpu.iff1
    );
    println!("  vdp regs {:02X?}", m.bus.vdp.regs);
    println!("  audio    {} samples, peak {peak}", audio.len());
    if let Some(p) = &m.bus.probe {
        for (a, e) in &p.entries {
            let callers: Vec<String> = e.callers.iter().map(|c| format!("{c:04X}")).collect();
            println!("  probe    entry {a:04X} x{} from {}", e.calls, callers.join(","));
        }
        let reads: Vec<String> = p.data_reads.iter().map(|(a, n)| format!("{a:04X}x{n}")).collect();
        println!("  probe    BIOS data reads: {}", reads.join(" "));
    }
    if mode == BiosMode::Hle {
        let fmt = |m: &std::collections::BTreeMap<u16, u64>| {
            m.iter().map(|(a, n)| format!("{a:04X}x{n}")).collect::<Vec<_>>().join(" ")
        };
        println!("  hle      unwritten routines called: {}", fmt(&m.hle_log.unimplemented));
        println!("  hle      wild BIOS addresses reached: {}", fmt(&m.hle_log.wild));
        for (a, from) in &m.hle_log.wild_from {
            println!("  hle      wild {a:04X} first reached from {from:04X}");
        }
    }

    if let Some(out) = save_to {
        std::fs::write(&out, m.save_state()).expect("write state");
        println!("  wrote    {out} (save state)");
    }
    if let Some(out) = vram_out {
        std::fs::write(&out, &m.bus.vdp.vram[..]).expect("write vram");
        println!("  wrote    {out} (VRAM)");
    }
    if let Some(out) = png {
        let rgb: Vec<u8> = m.framebuffer().iter().flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8]).collect();
        if let Err(e) = write_png(&out, WIDTH as u32, HEIGHT as u32, &rgb) {
            eprintln!("{out}: {e}");
            std::process::exit(1);
        }
        println!("  wrote    {out}");
    }
}

/// `FRAME:K`, with K a digit, `*` or `#`.
fn parse_key(s: &str) -> (u32, u8) {
    let (f, k) = s.split_once(':').unwrap_or_else(|| usage());
    let frame = f.parse().unwrap_or_else(|_| usage());
    let key = match k {
        "*" => 10,
        "#" => 11,
        d => d.parse().ok().filter(|&n: &u8| n <= 9).unwrap_or_else(|| usage()),
    };
    (frame, key)
}
