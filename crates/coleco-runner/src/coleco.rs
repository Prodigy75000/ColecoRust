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
//! frames from FRAME, and can be given more than once.

use coleco_core::machine::{Coleco, Firmware, Pad};
use coleco_core::vdp::{HEIGHT, WIDTH};
use coleco_core::{BiosMode, CPU_HZ, SAVE_STATE_VERSION};

fn usage() -> ! {
    eprintln!("usage: coleco [--bios PATH] [--frames N] [--png OUT] [--key FRAME:K]... CART");
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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bios" => bios = Some(args.next().unwrap_or_else(|| usage())),
            "--frames" => frames = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--png" => png = Some(args.next().unwrap_or_else(|| usage())),
            "--key" => keys.push(parse_key(&args.next().unwrap_or_else(|| usage()))),
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

    for f in 0..frames {
        let key = keys.iter().find(|&&(at, _)| f >= at && f < at + 10).map(|&(_, k)| k);
        m.bus.pads[0] = Pad { key, ..Pad::default() };
        m.run_frame();
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

/// A minimal RGB PNG: one IDAT of stored (uncompressed) deflate blocks. Big,
/// but dependency-free, and a dev harness does not care about size.
fn write_png(path: &str, w: u32, h: u32, rgb: &[u8]) -> std::io::Result<()> {
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for row in rgb.chunks(w as usize * 3) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    while let Some(c) = chunks.next() {
        z.push(if chunks.peek().is_none() { 1 } else { 0 });
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", &ihdr), (b"IDAT", &z), (b"IEND", &Vec::new())] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    std::fs::write(path, out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}
