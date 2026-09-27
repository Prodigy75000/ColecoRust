// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! `smoke`: run every cartridge under a directory and write one ledger row each.
//!
//!   smoke --bios bios/coleco.rom --out out/smoke dumps/corpus
//!   smoke --hle --out out/smoke-hle dumps/corpus
//!
//! `--hle` runs on ColecoRust's own BIOS instead of a dump, and the last two
//! ledger columns then count the distinct unwritten routines the title called
//! and the distinct BIOS addresses it reached that are no routine at all.
//!
//! Every title gets the same script: 1500 frames (25 s), keypad 1 held for ten
//! frames at 760, 1000 and 1240 (the first just after the title screen's
//! hand-over at about 670; "1" is the usual "skill 1, one player"), and both
//! fire buttons for ten frames at 880 and 1120. One press was not enough: a
//! game that reaches its menu late, or wants a key released first, never saw
//! it and sat on the menu, which is a script result, not a core one. Then
//! the run is measured and the verdict DERIVED from the measurements, from a
//! fixed vocabulary, never typed:
//!
//! | verdict | derived from |
//! |---|---|
//! | `LOAD_ERROR` | the machine would not build (oversized cartridge) |
//! | `NO_CART` | the CPU was never seen executing cartridge code |
//! | `CRASHED` | ended executing open bus, or halted with no interrupt that could wake it |
//! | `BLANK` | display off, or one colour on screen, at the end, AND no change over the last 100 frames |
//! | `STATIC` | picture unchanged over the last 100 frames and no sound |
//! | `ALIVE` | none of the above |
//!
//! `ALIVE` is a candidate, not a verdict on playability: a headless run cannot
//! tell a working game from a nicely animated wrong one. The PNGs and contact
//! sheets are there for the eye.

use coleco_core::machine::{Coleco, Firmware};
use corpus::{classify, header, FRAMES};
use coleco_core::vdp::{HEIGHT, WIDTH};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[path = "png.rs"]
mod png;
#[path = "corpus.rs"]
mod corpus;

const STILL_WINDOW: u32 = 100;
/// Thumbnails on a contact sheet, eight across.
const SHEET: usize = 48;

struct Row {
    file: String,
    class: &'static str,
    dump: &'static str,
    title: String,
    crc: u32,
    size: usize,
    header: &'static str,
    verdict: &'static str,
    first_cart_frame: Option<u32>,
    nmis: u64,
    colours: usize,
    moving: bool,
    peak: u16,
    pc: u16,
    hle_unimplemented: usize,
    hle_wild: usize,
    /// The unwritten routines and wild addresses, as `ADDR,ADDR`.
    hle_list: String,
    frame: Vec<u32>,
}

fn run_one(bios: Option<&[u8]>, path: &Path, root: &Path) -> Row {
    let file = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
    let cart = std::fs::read(path).unwrap_or_default();
    let (class, dump, title) = classify(&file);
    let mut row = Row {
        class,
        dump,
        title,
        file,
        crc: png::crc32(&cart),
        size: cart.len(),
        header: header(&cart),
        verdict: "LOAD_ERROR",
        first_cart_frame: None,
        nmis: 0,
        colours: 0,
        moving: false,
        peak: 0,
        pc: 0,
        hle_unimplemented: 0,
        hle_wild: 0,
        hle_list: String::new(),
        frame: vec![0xff00_0000; WIDTH * HEIGHT],
    };
    let firmware = match bios {
        Some(b) => Coleco::bios_from_bytes(b).expect("bios size checked in main"),
        None => Firmware::Hle,
    };
    let Ok(mut m) = Coleco::new(firmware, &cart) else { return row };

    let mut before_still: Vec<u32> = Vec::new();
    for f in 0..FRAMES {
        m.bus.pads = corpus::pads_at(f);
        m.run_frame();
        if row.first_cart_frame.is_none() && m.cpu.pc >= 0x8000 {
            row.first_cart_frame = Some(f);
        }
        let audio = m.take_audio();
        if f >= corpus::FIRST_INPUT {
            let p = audio.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            row.peak = row.peak.max(p);
        }
        if f == FRAMES - STILL_WINDOW {
            before_still = m.framebuffer().to_vec();
        }
    }

    row.frame = m.framebuffer().to_vec();
    row.moving = before_still != row.frame;
    let mut seen: Vec<u32> = row.frame.clone();
    seen.sort_unstable();
    seen.dedup();
    row.colours = seen.len();
    row.nmis = m.nmis;
    row.pc = m.cpu.pc;
    row.hle_unimplemented = m.hle_log.unimplemented.len();
    row.hle_wild = m.hle_log.wild.len();
    row.hle_list = m
        .hle_log
        .unimplemented
        .keys()
        .chain(m.hle_log.wild.keys())
        .map(|a| format!("{a:04X}"))
        .collect::<Vec<_>>()
        .join(",");

    let display_on = m.bus.vdp.regs[1] & 0x40 != 0;
    let open_bus = (0x2000..0x6000).contains(&m.cpu.pc);
    // HALT with interrupts disabled is NOT dead here: the VDP interrupt is the
    // NMI, which DI does not mask, and waiting for it that way is a normal
    // idiom. The first smoke run called four live games CRASHED for it. Dead
    // is a HALT that neither the NMI (VDP IE off) nor INT (IFF1 off) can end.
    let nmi_possible = m.bus.vdp.regs[1] & 0x20 != 0;
    let dead_halt = m.cpu.halted && !m.cpu.iff1 && !nmi_possible;
    row.verdict = if row.first_cart_frame.is_none() {
        "NO_CART"
    } else if open_bus || dead_halt {
        "CRASHED"
    } else if (!display_on || row.colours <= 1) && !row.moving {
        // A blank screen that is still changing is a transition caught at
        // the wrong instant: the first runs called Zaxxon and Looping BLANK
        // for that, and both play.
        "BLANK"
    } else if !row.moving && row.peak == 0 {
        "STATIC"
    } else {
        "ALIVE"
    };
    row
}

fn main() {
    let mut bios_path: Option<String> = None;
    let mut hle = false;
    let mut out = String::from("out/smoke");
    let mut dir: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bios" => bios_path = args.next(),
            "--hle" => hle = true,
            "--out" => out = args.next().unwrap_or(out),
            _ => dir = Some(a),
        }
    }
    let Some(dir) = dir.filter(|_| hle != bios_path.is_some()) else {
        eprintln!("usage: smoke (--bios PATH | --hle) [--out DIR] CARTDIR");
        std::process::exit(2);
    };
    let bios = bios_path.map(|p| std::fs::read(&p).expect("read bios"));
    if let Some(b) = &bios {
        Coleco::bios_from_bytes(b).expect("an 8 KB BIOS");
    }
    let root = PathBuf::from(&dir);
    let paths = corpus::cartridges(&root);
    std::fs::create_dir_all(&out).expect("create out dir");

    let jobs = Arc::new(Mutex::new(paths.clone().into_iter().enumerate().collect::<Vec<_>>()));
    let results = Arc::new(Mutex::new(Vec::new()));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let bios = Arc::new(bios);
    let root = Arc::new(root);
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (jobs, results, bios, root) = (jobs.clone(), results.clone(), bios.clone(), root.clone());
            std::thread::spawn(move || loop {
                let Some((i, p)) = jobs.lock().unwrap().pop() else { break };
                let row = run_one(bios.as_deref(), &p, &root);
                results.lock().unwrap().push((i, row));
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
    let mut rows = std::mem::take(&mut *results.lock().unwrap());
    rows.sort_by_key(|(i, _)| *i);

    let mut tsv = String::from(
        "n\tverdict\tclass\tdump\ttitle\tfile\tcrc32\tsize\theader\tfirst_cart_frame\tnmis\tcolours\tmoving\tpeak\tend_pc\thle_unimplemented\thle_wild\thle_addresses\n",
    );
    for (i, r) in &rows {
        tsv.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{:08x}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:04x}\t{}\t{}\t{}\n",
            i,
            r.verdict,
            r.class,
            r.dump,
            r.title,
            r.file,
            r.crc,
            r.size,
            r.header,
            r.first_cart_frame.map_or("-".into(), |f| f.to_string()),
            r.nmis,
            r.colours,
            r.moving,
            r.peak,
            r.pc,
            r.hle_unimplemented,
            r.hle_wild,
            r.hle_list
        ));
    }
    std::fs::write(format!("{out}/smoke.tsv"), &tsv).expect("write tsv");

    // Contact sheets: 48 thumbnails at half size, eight across, in ledger order.
    for (s, chunk) in rows.chunks(SHEET).enumerate() {
        let (tw, th) = (WIDTH / 2, HEIGHT / 2);
        let (cols, rows_n) = (8, SHEET / 8);
        let (w, h) = (cols * (tw + 2), rows_n * (th + 2));
        let mut rgb = vec![40u8; w * h * 3];
        for (k, (_, r)) in chunk.iter().enumerate() {
            let (ox, oy) = ((k % cols) * (tw + 2), (k / cols) * (th + 2));
            for y in 0..th {
                for x in 0..tw {
                    let p = r.frame[(y * 2) * WIDTH + x * 2];
                    let o = ((oy + y) * w + ox + x) * 3;
                    rgb[o..o + 3].copy_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
                }
            }
        }
        png::write_png(&format!("{out}/sheet-{s:02}.png"), w as u32, h as u32, &rgb).expect("write sheet");
    }

    const ORDER: [&str; 6] = ["ALIVE", "STATIC", "BLANK", "CRASHED", "NO_CART", "LOAD_ERROR"];
    println!("{} files under {dir}", rows.len());
    for v in ORDER {
        println!("  {v:<10} {}", rows.iter().filter(|(_, r)| r.verdict == v).count());
    }
    // Per title: its best verdict over the dumps not known to be bad. A title
    // whose only dumps are bad is counted apart, not as a failure.
    for class in ["commercial", "pd"] {
        let mut titles: std::collections::BTreeMap<&str, Option<usize>> = Default::default();
        for (_, r) in rows.iter().filter(|(_, r)| r.class == class) {
            let e = titles.entry(r.title.as_str()).or_insert(None);
            if r.dump != "bad" {
                let rank = ORDER.iter().position(|&v| v == r.verdict).unwrap();
                *e = Some(e.map_or(rank, |b| b.min(rank)));
            }
        }
        let only_bad = titles.values().filter(|b| b.is_none()).count();
        println!("{class}: {} titles, {only_bad} with only bad dumps", titles.len());
        for (k, v) in ORDER.iter().enumerate() {
            let n = titles.values().filter(|b| **b == Some(k)).count();
            if n > 0 {
                println!("  {v:<10} {n}");
            }
        }
    }
    println!("ledger: {out}/smoke.tsv, contact sheets: {out}/sheet-NN.png");
}
