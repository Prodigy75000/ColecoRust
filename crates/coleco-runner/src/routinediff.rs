// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! `routinediff`: one HLE routine against the real one, on real calls.
//!
//!   routinediff --bios bios/coleco.rom --routine 1FD9 [--show KIND] dumps/corpus
//!   routinediff --bios bios/coleco.rom --routine all --title Frenzy dumps/corpus
//!
//! `--show RAM` (or any difference kind) lists only examples with that kind.
//! `--routine all` compares every routine in the jump table at once, one
//! tally each. `--title TEXT` keeps only titles whose name contains TEXT and
//! then compares every call, not a sample: the way to find which routine a
//! title that fails on the HLE, but calls nothing unwritten, is tripping on.
//!
//! Every title (one dump each) runs on the REAL BIOS under the smoke script.
//! Whenever cartridge code reaches the routine (its jump-table slot, or the
//! routine's own address for games that call it directly), the whole machine
//! is snapshotted, the real routine runs to its return, and the result is
//! kept. The snapshot is then loaded into a machine on the HLE, which runs its
//! version to the same return, and the two are compared: CPU registers, RAM,
//! VRAM, the VDP's registers and internal state, and the PSG.
//!
//! This is the oracle rule as a tool: the real BIOS is right until proven
//! otherwise, and every call a game actually made is a test case.
//!
//! Samples during which an NMI arrived are counted and set aside: the game's
//! own handler ran in the middle of them and the two runs cannot line up.
//!
//! Calls into the object system (ACTIVATE, PUTOBJ) are also tallied by object
//! type, the low nibble of the object's first graphics byte, because each type
//! is its own routine inside the BIOS and they are written one at a time.

use coleco_core::hle::TABLE;
use coleco_core::machine::{Coleco, Firmware};
use coleco_core::save::{SaveState, WriteCursor};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[path = "corpus.rs"]
mod corpus;

/// Calls sampled per title: the first few, then a thinner spread.
const FIRST_SAMPLES: u32 = 12;
const EVERY: u32 = 97;
const MAX_SAMPLES: u32 = 40;
/// With `--title`, every call, up to this many per routine.
const ALL_SAMPLES: u32 = 5000;
/// A call that has not returned in this many cycles is abandoned (a routine
/// entered by JP, or one that never returns).
const TIMEOUT: u64 = 4_000_000;

/// Back from a call made with SP at `sp0` to return to `ret`: the return
/// address popped and the CPU there. A P entry returns past the parameters
/// written after its CALL, so for those, anywhere in the cartridge will do;
/// for the rest the address must match, or a call that never comes back
/// the ordinary way (a tail jump) would be compared at different points on
/// the two machines.
fn returned(m: &Coleco, sp0: u16, ret: u16, p_entry: bool) -> bool {
    m.cpu.sp == sp0.wrapping_add(2) && if p_entry { m.cpu.pc >= 0x8000 } else { m.cpu.pc == ret }
}

fn save<T: SaveState>(x: &T) -> Vec<u8> {
    let mut w = WriteCursor::new();
    x.save(&mut w);
    w.into_bytes()
}

/// The differences between two machines, as short labels.
fn differences(real: &Coleco, hle: &Coleco) -> Vec<String> {
    let mut d = Vec::new();
    let (r, h) = (&real.cpu, &hle.cpu);
    let regs = [
        ("A", r.a() as u16, h.a() as u16),
        ("F", r.f as u16, h.f as u16),
        ("BC", r.bc(), h.bc()),
        ("DE", r.de(), h.de()),
        ("HL", r.hl(), h.hl()),
        ("IX", r.ix, h.ix),
        ("IY", r.iy, h.iy),
        ("SP", r.sp, h.sp),
        ("PC", r.pc, h.pc),
    ];
    for (name, a, b) in regs {
        if a != b {
            d.push(format!("{name} {a:04X}/{b:04X}"));
        }
    }
    // RAM just below the stack pointer is what the routine pushed and popped
    // while it worked: garbage once it returns, and the HLE pushes nothing.
    // Reported apart as STACK so it cannot hide a real RAM difference.
    let sp = real.cpu.sp as usize & 0x3ff;
    let below_sp = |i: usize| (sp.wrapping_sub(i) & 0x3ff) <= 48 && i != sp;
    let all: Vec<usize> = (0..real.bus.ram.len()).filter(|&i| real.bus.ram[i] != hle.bus.ram[i]).collect();
    let (stack, ram): (Vec<usize>, Vec<usize>) = all.into_iter().partition(|&i| below_sp(i));
    if !stack.is_empty() {
        d.push(format!("STACK x{}", stack.len()));
    }
    if !ram.is_empty() {
        let shown: Vec<String> = ram
            .iter()
            .take(8)
            .map(|&i| format!("{:04X} {:02X}/{:02X}", 0x7000 + i, real.bus.ram[i], hle.bus.ram[i]))
            .collect();
        d.push(format!("RAM x{} {}", ram.len(), shown.join(" ")));
    }
    let (rv, hv) = (&real.bus.vdp.vram, &hle.bus.vdp.vram);
    let vram: Vec<usize> = (0..rv.len()).filter(|&i| rv[i] != hv[i]).collect();
    if !vram.is_empty() {
        d.push(format!("VRAM x{} first {:04X} {:02X}/{:02X}", vram.len(), vram[0], rv[vram[0]], hv[vram[0]]));
    }
    // The VDP's state after its VRAM: registers, address, latch, buffer,
    // status. Not the scanline counter (the last two bytes), which is where
    // the machine is in time, not something a routine sets.
    let (rs, hs) = (save(&real.bus.vdp), save(&hle.bus.vdp));
    let (rs, hs) = (&rs[..rs.len() - 2], &hs[..hs.len() - 2]);
    if rs[0x4000..] != hs[0x4000..] {
        if real.bus.vdp.regs != hle.bus.vdp.regs {
            d.push(format!("VDP regs {:02X?}/{:02X?}", real.bus.vdp.regs, hle.bus.vdp.regs));
        } else if rs[0x4008..0x400d] != hs[0x4008..0x400d] {
            d.push(format!("VDP state {:02X?}/{:02X?}", &rs[0x4008..], &hs[0x4008..]));
        } else {
            // Only the status flags and fifth-sprite number: set by lines
            // being drawn, which depends on when, not on the routine.
            d.push(format!("VDPSTATUS {:02X?}/{:02X?}", &rs[0x400d..], &hs[0x400d..]));
        }
    }
    // PSG registers only: its counters run with time, and a routine that
    // took a different number of cycles would differ there for no fault.
    let (rp, hp) = (real.bus.audio.psg.registers(), hle.bus.audio.psg.registers());
    if rp != hp {
        d.push(format!("PSG {rp:03X?}/{hp:03X?}"));
    }
    d
}

#[derive(Default)]
struct Tally {
    samples: u64,
    exact: u64,
    /// Exact apart from STACK and VDPSTATUS, the two kinds that are not the
    /// routine's doing.
    clean: u64,
    nmi_skipped: u64,
    timeouts: u64,
    real_cycles: u64,
    hle_cycles: u64,
    /// (real, HLE) cycles per sample, for the timing fit, with the title
    /// and entry registers to name the worst call.
    timings: Vec<(u64, u64, String)>,
    /// Difference kind (the label up to its first space) to count.
    kinds: BTreeMap<String, u64>,
    examples: Vec<String>,
    /// Object type to (samples, clean, titles), for the object routines.
    classes: BTreeMap<u8, (u64, u64, BTreeSet<String>)>,
}

/// The object type a call to ACTIVATE or PUTOBJ works on, or None for any
/// other routine. ACTIVATE takes HL pointing at the object's descriptor, whose
/// first word is its graphics; PUTOBJ takes IX pointing at the same; WRITER
/// is tallied by the first object in its queue.
fn object_type(m: &Coleco, target: u16) -> Option<u8> {
    let word = |a: u16| u16::from_le_bytes([m.bus.peek(a), m.bus.peek(a.wrapping_add(1))]);
    let descriptor = match target {
        0x04a3 => m.cpu.hl(),
        0x06d8 => m.cpu.ix,
        // WRITER: the object at the head of its queue, if there is one.
        0x0679 if m.bus.peek(0x73cc) != m.bus.peek(0x73cb) => word(word(0x73cf)),
        _ => return None,
    };
    Some(m.bus.peek(word(descriptor)) & 0x0f)
}

/// Compare the calls one title makes to any of `routines` (slot, routine
/// pairs), into one tally per routine.
fn run_title(
    bios: &[u8],
    cart: &[u8],
    title: &str,
    routines: &[(u16, u16)],
    show: &str,
    every_call: bool,
    tallies: &mut BTreeMap<u16, Tally>,
) {
    let Ok(mut m) = Coleco::new(Coleco::bios_from_bytes(bios).unwrap(), cart) else { return };
    let Ok(mut h) = Coleco::new(Firmware::Hle, cart) else { return };
    let mut seen: BTreeMap<u16, u32> = BTreeMap::new();
    let mut frame = 0u32;
    let mut was_bios = false;
    while frame < corpus::FRAMES {
        m.bus.pads = corpus::pads_at(frame);
        let pc = m.cpu.pc;
        // Entered from outside the BIOS, at the slot or the routine itself.
        let called = if was_bios { None } else { routines.iter().find(|&&(s, t)| pc == s || pc == t) };
        if let Some(&(_, target)) = called {
            let seen = seen.entry(target).or_default();
            *seen += 1;
            let seen = *seen;
            let t = tallies.entry(target).or_default();
            let take = if every_call {
                seen <= ALL_SAMPLES
            } else {
                seen <= FIRST_SAMPLES || (seen % EVERY == 0 && seen / EVERY < MAX_SAMPLES)
            };
            if take {
                let before = m.save_state();
                let entry = format!(
                    "[A={:02X} BC={:04X} DE={:04X} HL={:04X} IY={:04X}]",
                    m.cpu.a(),
                    m.cpu.bc(),
                    m.cpu.de(),
                    m.cpu.hl(),
                    m.cpu.iy
                );
                let pads = m.bus.pads;
                let class = object_type(&m, target);
                let sp0 = m.cpu.sp;
                let ret = u16::from_le_bytes([m.bus.peek(sp0), m.bus.peek(sp0.wrapping_add(1))]);
                let p_entry = coleco_core::hle::pvariant::is_entry(target);
                let (c0, n0) = (m.cycles(), m.nmis);
                while !returned(&m, sp0, ret, p_entry) && m.cycles() - c0 < TIMEOUT {
                    m.step();
                }
                if m.cycles() - c0 >= TIMEOUT {
                    t.timeouts += 1;
                } else if m.nmis != n0 {
                    t.nmi_skipped += 1;
                } else {
                    t.samples += 1;
                    t.real_cycles += m.cycles() - c0;
                    h.load_state(&before).expect("state from the same build");
                    h.bus.pads = pads;
                    let hc0 = h.cycles();
                    while !returned(&h, sp0, ret, p_entry) && h.cycles() - hc0 < TIMEOUT {
                        h.step();
                    }
                    t.hle_cycles += h.cycles() - hc0;
                    t.timings.push((m.cycles() - c0, h.cycles() - hc0, format!("{title} frame {frame} {entry}")));
                    let d = differences(&m, &h);
                    let timing_only = |k: &String| k.starts_with("STACK") || k.starts_with("VDPSTATUS");
                    if d.is_empty() {
                        t.exact += 1;
                    }
                    if d.iter().all(timing_only) {
                        t.clean += 1;
                    }
                    if let Some(c) = class {
                        let e = t.classes.entry(c).or_default();
                        e.0 += 1;
                        e.1 += u64::from(d.iter().all(timing_only));
                        e.2.insert(title.to_string());
                    }
                    if !d.is_empty() {
                        for k in &d {
                            let kind = k.split(' ').next().unwrap_or(k).to_string();
                            *t.kinds.entry(kind).or_default() += 1;
                        }
                        let wanted = if show.is_empty() { !d.iter().all(timing_only) } else { d.iter().any(|k| k.starts_with(show)) };
                        if t.examples.len() < 6 && wanted {
                            let entry = match class {
                                Some(c) => format!("type {c} {entry}"),
                                None => entry.clone(),
                            };
                            t.examples.push(format!("{title} frame {frame} {entry}: {}", d.join("; ")));
                        }
                    }
                }
            }
        }
        was_bios = m.cpu.pc < 0x2000;
        m.step();
        if !m.in_line() && m.bus.vdp.line == 0 {
            frame += 1;
            m.take_audio();
        }
    }
}

/// Real cycles against the HLE's, per call: the least-squares line
/// real = a * hle + b and how far calls sit from it. A slope near 1 means
/// the HLE is off by a constant (b); another slope, by a cost per unit of
/// work. What the timing calibration is read from.
fn fit(samples: &[(u64, u64, String)]) -> String {
    let n = samples.len() as f64;
    if samples.is_empty() {
        return "no samples".to_string();
    }
    let (sx, sy) = samples.iter().fold((0.0, 0.0), |(x, y), (r, h, _)| (x + *h as f64, y + *r as f64));
    let (mx, my) = (sx / n, sy / n);
    let (sxx, sxy) = samples.iter().fold((0.0, 0.0), |(xx, xy), (r, h, _)| {
        (xx + (*h as f64 - mx).powi(2), xy + (*h as f64 - mx) * (*r as f64 - my))
    });
    let a = if sxx > 0.0 { sxy / sxx } else { 1.0 };
    let b = my - a * mx;
    let (lo, hi) = samples.iter().fold((u64::MAX, 0), |(lo, hi), (r, _, _)| (lo.min(*r), hi.max(*r)));
    // The call furthest from equal time, real against HLE.
    let (wr, wh, who) = samples.iter().max_by_key(|(r, h, _)| r.abs_diff(*h)).unwrap();
    format!(
        "real = {a:.3} x hle + {b:.0}; real calls {lo} to {hi}\n    worst: real {wr}, HLE {wh}: {who}"
    )
}

fn main() {
    let mut bios_path: Option<String> = None;
    let mut routine: Option<String> = None;
    let mut show = String::new();
    let mut only_title: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bios" => bios_path = args.next(),
            "--routine" => routine = args.next(),
            "--show" => show = args.next().unwrap_or_default(),
            "--title" => only_title = args.next(),
            _ => dir = Some(a),
        }
    }
    let (Some(bios_path), Some(routine), Some(dir)) = (bios_path, routine, dir) else {
        eprintln!("usage: routinediff --bios PATH --routine SLOT_HEX|all [--title TEXT] [--show KIND] CARTDIR");
        std::process::exit(2);
    };
    // The jump table, and the addresses games call directly inside the BIOS
    // (compared as a routine whose slot is its own address).
    let known: Vec<(u16, u16)> =
        TABLE.iter().copied().chain(coleco_core::hle::routines::ENTRY_POINTS.iter().map(|&a| (a, a))).collect();
    let routines: Vec<(u16, u16)> = if routine == "all" {
        known
    } else {
        let wanted = u16::from_str_radix(&routine, 16).unwrap_or(0);
        let pair = known.iter().copied().find(|&(s, t)| s == wanted || t == wanted).unwrap_or_else(|| {
            eprintln!("{routine} is not a jump-table slot or routine");
            std::process::exit(2)
        });
        vec![pair]
    };
    let routines = Arc::new(routines);
    // The real BIOS runs with OUR data from $143B to $18D3 patched in: the
    // font, and the title screen's logo, colours and layout, which the image
    // leaves out (an empty layout list). So a routine that moves them (LOAD_
    // ASCII, a game copying glyphs or the logo) compares equal when it moves
    // them correctly, instead of differing by design on every byte. lockstep
    // patches the same range.
    let mut bios = std::fs::read(&bios_path).expect("read bios");
    let ours = coleco_core::hle::image();
    bios[0x143b..=0x18d3].copy_from_slice(&ours[0x143b..=0x18d3]);
    let bios = Arc::new(bios);
    let root = PathBuf::from(&dir);
    let mut paths = corpus::representatives(&corpus::cartridges(&root), &root);
    if let Some(want) = &only_title {
        paths.retain(|p| {
            let file = p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/");
            corpus::classify(&file).2.contains(want.as_str())
        });
    }
    let every_call = only_title.is_some();

    let jobs = Arc::new(Mutex::new(paths));
    let tally: Arc<Mutex<BTreeMap<u16, Tally>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (jobs, tally, bios, root, show) = (jobs.clone(), tally.clone(), bios.clone(), root.clone(), show.clone());
            let routines = routines.clone();
            std::thread::spawn(move || loop {
                let Some(p) = jobs.lock().unwrap().pop() else { break };
                let file = p.strip_prefix(&root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                let (_, _, title) = corpus::classify(&file);
                let cart = std::fs::read(&p).unwrap_or_default();
                let mut ts = BTreeMap::new();
                run_title(&bios, &cart, &title, &routines, &show, every_call, &mut ts);
                let mut tallies = tally.lock().unwrap();
                for (target, t) in ts {
                    let all = tallies.entry(target).or_default();
                    all.samples += t.samples;
                    all.exact += t.exact;
                    all.clean += t.clean;
                    all.nmi_skipped += t.nmi_skipped;
                    all.timeouts += t.timeouts;
                    all.real_cycles += t.real_cycles;
                    all.hle_cycles += t.hle_cycles;
                    all.timings.extend(t.timings);
                    for (k, v) in t.kinds {
                        *all.kinds.entry(k).or_default() += v;
                    }
                    for (c, (n, clean, titles)) in t.classes {
                        let e = all.classes.entry(c).or_default();
                        e.0 += n;
                        e.1 += clean;
                        e.2.extend(titles);
                    }
                    for e in t.examples {
                        if all.examples.len() < 12 {
                            all.examples.push(e);
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
    let tallies = tally.lock().unwrap();
    for &(slot, target) in routines.iter() {
        let Some(t) = tallies.get(&target) else { continue };
        println!("routine {slot:04X} -> {target:04X}");
        println!(
            "  {} samples, {} exact ({:.1}%), {} set aside for an NMI, {} timed out",
            t.samples,
            t.exact,
            if t.samples == 0 { 0.0 } else { t.exact as f64 * 100.0 / t.samples as f64 },
            t.nmi_skipped,
            t.timeouts
        );
        println!(
            "  {} clean ({:.1}%): exact but for stack left-overs and timing-set VDP status",
            t.clean,
            if t.samples == 0 { 0.0 } else { t.clean as f64 * 100.0 / t.samples as f64 }
        );
        if t.samples > 0 {
            println!(
                "  real routine: {} cycles per call on average, the HLE {}",
                t.real_cycles / t.samples,
                t.hle_cycles / t.samples
            );
            println!("  timing: {}", fit(&t.timings));
        }
        for (k, v) in &t.kinds {
            println!("  differs in {k:<6} {v}");
        }
        for (c, (n, clean, titles)) in &t.classes {
            let names: Vec<&str> = titles.iter().map(String::as_str).collect();
            println!("  object type {c}: {clean}/{n} clean, {} titles: {}", titles.len(), names.join("; "));
        }
        for e in &t.examples {
            println!("  e.g. {e}");
        }
    }
}
