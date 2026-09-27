// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! `lockstep`: one title on the real BIOS and on the HLE side by side, from
//! the same machine, until they first disagree.
//!
//!   lockstep --bios bios/coleco-nodelay.rom [--frames N] [--key FRAME:K]... CART
//!
//! Both machines are built on the cartridge, the real one is run to the
//! cartridge's first instruction, and its state is loaded into the HLE one,
//! so they start identical. Then each is run to the end of its next BIOS call
//! made from cartridge code, and the two calls are compared: where they went,
//! the registers going in and coming out, and all of RAM (but for the stack
//! below SP, which is scratch) and the VDP registers afterwards. The first
//! call that differs is printed with the calls before it.
//!
//! The comparison is by call, not by frame: a long real routine such as
//! FILL_VRAM runs across a frame's end while the HLE's runs in one step, so
//! frames do not line up but calls do. An interrupt can still land at a
//! different point on each side (the HLE's routines take their time all at
//! once), which shows up as the calls of an NMI handler out of order: that is
//! timing, not the fault, and `--skip-nmi` leaves those calls out.
//!
//! routinediff compares one call at a time from a shared state and cannot see
//! a difference that builds up over calls or is not a call at all (a read of
//! BIOS data, an interrupt landing elsewhere). This can; it is the tool for a
//! title that fails on the HLE although every call routinediff samples is
//! clean. Use the no-delay BIOS: the delay only costs time here.
//!
//! `--ignore ADDR` (hex, repeatable) leaves a RAM byte out of the comparison:
//! a frame counter an NMI bumps at a different moment on each side, say.
//!
//! `--state FILE` starts both from a save state instead (`coleco
//! --save-state` writes one): the way past a stretch where the two drift
//! apart on timing alone, to the moment that matters, a key press say.
//!
//! The real BIOS runs with the HLE image's data from `$143B` to `$18D3` (the
//! title screen's logo, colours and layout, the font) patched in, as
//! routinediff patches the font: the HLE carries its own font and no logo on
//! purpose, and a game that copies them would otherwise differ there first
//! and hide the difference being looked for.

use coleco_core::machine::{Coleco, Firmware, Pad};

fn usage() -> ! {
    eprintln!("usage: lockstep --bios PATH [--frames N] [--key FRAME:K]... [--skip-nmi] [--state FILE] [--ignore ADDR]... CART");
    std::process::exit(2);
}

fn regs(m: &Coleco) -> String {
    format!(
        "A={:02X} F={:02X} BC={:04X} DE={:04X} HL={:04X} IX={:04X} IY={:04X} SP={:04X}",
        m.cpu.a(),
        m.cpu.f,
        m.cpu.bc(),
        m.cpu.de(),
        m.cpu.hl(),
        m.cpu.ix,
        m.cpu.iy,
        m.cpu.sp
    )
}

/// One machine and where it is in the input script.
struct Side {
    m: Coleco,
    frame: u32,
    /// Inside the NMI handler: the SP the handler's RETN will restore.
    in_nmi: Option<u16>,
}

/// A BIOS call seen from the cartridge.
struct Call {
    target: u16,
    entry: String,
    exit: String,
    frame: u32,
    in_nmi: bool,
}

impl Side {
    fn step(&mut self, keys: &[(u32, u8)]) {
        let key = keys.iter().find(|&&(at, _)| self.frame >= at && self.frame < at + 10).map(|&(_, k)| k);
        let pad = Pad { key, ..Pad::default() };
        self.m.bus.pads = [pad, pad];
        let nmis = self.m.nmis;
        let sp = self.m.cpu.sp;
        self.m.step();
        if self.m.nmis != nmis && self.in_nmi.is_none() {
            self.in_nmi = Some(sp);
        }
        if let Some(s) = self.in_nmi {
            if self.m.cpu.sp == s && self.m.cpu.pc >= 0x8000 {
                self.in_nmi = None;
            }
        }
        if !self.m.in_line() && self.m.bus.vdp.line == 0 {
            self.frame += 1;
        }
    }

    /// Run to the end of the next BIOS call made from cartridge code.
    fn next_call(&mut self, keys: &[(u32, u8)], frames: u32) -> Option<Call> {
        loop {
            if self.frame >= frames {
                return None;
            }
            let pc = self.m.cpu.pc;
            let from_cart = self.m.cpu.pc >= 0x8000;
            self.step(keys);
            let to = self.m.cpu.pc;
            if !(from_cart && to < 0x2000 && to != 0x0066) {
                continue;
            }
            let sp = self.m.cpu.sp;
            let ret = u16::from_le_bytes([self.m.bus.peek(sp), self.m.bus.peek(sp.wrapping_add(1))]);
            // A jump into the BIOS, not a call: nothing to wait for.
            if ret != pc.wrapping_add(3) && ret != pc.wrapping_add(1) {
                continue;
            }
            let entry = regs(&self.m);
            let (frame, in_nmi) = (self.frame, self.in_nmi.is_some());
            let mut guard = 0u64;
            // Back when the return address is popped and the CPU is in the
            // cartridge: P entries return past the words after their CALL.
            while !(self.m.cpu.sp == sp.wrapping_add(2) && self.m.cpu.pc >= 0x8000) && guard < 20_000_000 {
                self.step(keys);
                guard += 1;
            }
            return Some(Call { target: to, entry, exit: regs(&self.m), frame, in_nmi });
        }
    }
}

fn describe(c: &Call) -> String {
    format!(
        "{:04X} frame {}{}\n        in  {}\n        out {}",
        c.target,
        c.frame,
        if c.in_nmi { " (in NMI)" } else { "" },
        c.entry,
        c.exit
    )
}

fn main() {
    let mut bios: Option<String> = None;
    let mut frames = 1500u32;
    let mut keys: Vec<(u32, u8)> = Vec::new();
    let mut skip_nmi = false;
    let mut state: Option<String> = None;
    let mut ignore: Vec<usize> = Vec::new();
    let mut cart: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bios" => bios = args.next(),
            "--frames" => frames = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--skip-nmi" => skip_nmi = true,
            "--state" => state = args.next(),
            "--ignore" => {
                let v = args.next().and_then(|v| usize::from_str_radix(&v, 16).ok()).unwrap_or_else(|| usage());
                ignore.push(v & 0x3ff);
            }
            "--key" => {
                let v = args.next().unwrap_or_else(|| usage());
                let (f, k) = v.split_once(':').unwrap_or_else(|| usage());
                keys.push((f.parse().unwrap_or_else(|_| usage()), k.parse().unwrap_or_else(|_| usage())));
            }
            _ => cart = Some(a),
        }
    }
    let (Some(bios), Some(cart)) = (bios, cart) else { usage() };
    let mut bios = std::fs::read(&bios).expect("read bios");
    let ours = coleco_core::hle::image();
    bios[0x143b..=0x18d3].copy_from_slice(&ours[0x143b..=0x18d3]);
    let cart = std::fs::read(&cart).expect("read cart");
    let mut real = Coleco::new(Coleco::bios_from_bytes(&bios).unwrap(), &cart).unwrap();
    let mut hle = Coleco::new(Firmware::Hle, &cart).unwrap();
    match &state {
        Some(p) => {
            let bytes = std::fs::read(p).expect("read state");
            real.load_state(&bytes).expect("load state");
        }
        None => {
            let mut steps = 0u64;
            while real.cpu.pc < 0x8000 && steps < 40_000_000 {
                real.step();
                steps += 1;
            }
        }
    }
    hle.load_state(&real.save_state()).expect("same build");
    let mut r = Side { m: real, frame: 0, in_nmi: None };
    let mut h = Side { m: hle, frame: 0, in_nmi: None };
    let mut history: Vec<String> = Vec::new();
    let mut n = 0u64;
    loop {
        let next = |s: &mut Side| loop {
            match s.next_call(&keys, frames) {
                Some(c) if skip_nmi && c.in_nmi => continue,
                other => return other,
            }
        };
        let (Some(rc), Some(hc)) = (next(&mut r), next(&mut h)) else {
            println!("no difference in {n} calls ({frames} frames)");
            return;
        };
        n += 1;
        let sp = r.m.cpu.sp as usize & 0x3ff;
        let scratch = |i: usize| (sp.wrapping_sub(i) & 0x3ff) <= 48 && i < sp;
        let ram: Vec<usize> = (0..r.m.bus.ram.len())
            .filter(|&i| r.m.bus.ram[i] != h.m.bus.ram[i] && !scratch(i) && !ignore.contains(&i))
            .collect();
        let same_call = rc.target == hc.target && rc.entry == hc.entry;
        if same_call && rc.exit == hc.exit && ram.is_empty() && r.m.bus.vdp.regs == h.m.bus.vdp.regs {
            history.push(describe(&rc));
            if history.len() > 6 {
                history.remove(0);
            }
            continue;
        }
        println!("call {n}: first difference");
        for p in &history {
            println!("  before: {p}");
        }
        println!("  real  {}", describe(&rc));
        println!("  hle   {}", describe(&hc));
        if r.m.bus.vdp.regs != h.m.bus.vdp.regs {
            println!("  VDP regs real {:02X?} hle {:02X?}", r.m.bus.vdp.regs, h.m.bus.vdp.regs);
        }
        for &i in ram.iter().take(24) {
            println!("  RAM {:04X} real {:02X} hle {:02X}", 0x7000 + i, r.m.bus.ram[i], h.m.bus.ram[i]);
        }
        println!("  {} RAM bytes differ outside the stack", ram.len());
        return;
    }
}
