// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! `census`: what the corpus asks of the BIOS, measured on the real one.
//!
//!   census --bios bios/coleco.rom --out ledger dumps/corpus
//!
//! One representative dump per title (a good dump if there is one, never a
//! bad one), driven by the same script as `smoke`, with the core's
//! [`coleco_core::census::Probe`] attached. Four ledgers come out:
//!
//! - `census-entries.tsv`: every BIOS address cartridge code transferred to,
//!   with how many titles did, most-used first. This is the HLE's work list.
//! - `census-titles.tsv`: per title, the entries it used, how many BIOS bytes
//!   it read as data, how many BIOS-written RAM bytes it read, and the share
//!   of its running time spent in BIOS code.
//! - `census-ram.tsv`: the RAM contract, one row per byte the BIOS writes and
//!   games read: how many titles read it as the boot left it, how many read it
//!   as a BIOS routine left it during play, and the values it held at
//!   hand-over.
//! - `census-data-reads.tsv`: BIOS bytes read as data, with the titles.
//!
//! And a coverage curve on stdout: with the N most-used entries implemented,
//! how many titles have every entry they use covered. That turns "the
//! majority of titles" into a list with a count against it.
//!
//! What this cannot see: code paths the script never reaches. A routine only
//! called on level 3, or on a game over, is absent here. So a title "fully
//! covered" means covered for what 25 s of this script exercised.

use coleco_core::census::Probe;
use coleco_core::machine::Coleco;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[path = "corpus.rs"]
mod corpus;
#[path = "png.rs"]
mod png;

struct Title {
    title: String,
    class: &'static str,
    file: String,
    crc: u32,
    probe: Probe,
}

fn run(bios: &[u8], path: &Path, root: &Path) -> Option<Title> {
    let file = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
    let (class, _, title) = corpus::classify(&file);
    let cart = std::fs::read(path).ok()?;
    let mut m = Coleco::new(Coleco::bios_from_bytes(bios).ok()?, &cart).ok()?;
    m.bus.probe = Some(Box::new(Probe::new()));
    for f in 0..corpus::FRAMES {
        m.bus.pads = corpus::pads_at(f);
        m.run_frame();
        m.take_audio();
    }
    let probe = *m.bus.probe.take()?;
    Some(Title { title, class, file, crc: png::crc32(&cart), probe })
}

fn ranges(addrs: impl Iterator<Item = u16>) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run: Option<(u16, u16)> = None;
    for a in addrs {
        run = match run {
            Some((s, e)) if a == e + 1 => Some((s, a)),
            Some((s, e)) => {
                out.push(if s == e { format!("{s:04X}") } else { format!("{s:04X}-{e:04X}") });
                Some((a, a))
            }
            None => Some((a, a)),
        };
    }
    if let Some((s, e)) = run {
        out.push(if s == e { format!("{s:04X}") } else { format!("{s:04X}-{e:04X}") });
    }
    out.join(",")
}

fn main() {
    let mut bios_path: Option<String> = None;
    let mut out = String::from("ledger");
    let mut dir: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bios" => bios_path = args.next(),
            "--out" => out = args.next().unwrap_or(out),
            _ => dir = Some(a),
        }
    }
    let (Some(bios_path), Some(dir)) = (bios_path, dir) else {
        eprintln!("usage: census --bios PATH [--out DIR] CARTDIR");
        std::process::exit(2);
    };
    let bios = Arc::new(std::fs::read(&bios_path).expect("read bios"));
    Coleco::bios_from_bytes(&bios).expect("an 8 KB BIOS");
    let root = Arc::new(PathBuf::from(&dir));
    let paths = corpus::representatives(&corpus::cartridges(&root), &root);
    std::fs::create_dir_all(&out).expect("create out dir");

    let jobs = Arc::new(Mutex::new(paths));
    let results = Arc::new(Mutex::new(Vec::new()));
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (jobs, results, bios, root) = (jobs.clone(), results.clone(), bios.clone(), root.clone());
            std::thread::spawn(move || loop {
                let Some(p) = jobs.lock().unwrap().pop() else { break };
                if let Some(t) = run(&bios, &p, &root) {
                    results.lock().unwrap().push(t);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
    let mut titles = std::mem::take(&mut *results.lock().unwrap());
    titles.sort_by(|a, b| (a.class, &a.title).cmp(&(b.class, &b.title)));

    // ---- entries ----
    struct Agg {
        commercial: usize,
        pd: usize,
        calls: u64,
        callers: BTreeSet<u16>,
        examples: Vec<String>,
    }
    let mut entries: BTreeMap<u16, Agg> = BTreeMap::new();
    for t in &titles {
        for (&addr, e) in &t.probe.entries {
            let a = entries.entry(addr).or_insert(Agg {
                commercial: 0,
                pd: 0,
                calls: 0,
                callers: BTreeSet::new(),
                examples: Vec::new(),
            });
            if t.class == "pd" {
                a.pd += 1;
            } else {
                a.commercial += 1;
            }
            a.calls += e.calls;
            a.callers.extend(e.callers.iter().copied().take(2));
            if a.examples.len() < 3 {
                a.examples.push(t.title.clone());
            }
        }
    }
    let mut ranked: Vec<(&u16, &Agg)> = entries.iter().collect();
    ranked.sort_by(|a, b| (b.1.commercial + b.1.pd, b.1.calls).cmp(&(a.1.commercial + a.1.pd, a.1.calls)).then(a.0.cmp(b.0)));
    let mut tsv = String::from("rank\tentry\ttitles_commercial\ttitles_pd\tcalls\tsample_callers\tsample_titles\n");
    for (i, (addr, a)) in ranked.iter().enumerate() {
        let callers: Vec<String> = a.callers.iter().take(6).map(|c| format!("{c:04X}")).collect();
        tsv.push_str(&format!(
            "{}\t{:04X}\t{}\t{}\t{}\t{}\t{}\n",
            i + 1,
            addr,
            a.commercial,
            a.pd,
            a.calls,
            callers.join(","),
            a.examples.join("; ")
        ));
    }
    std::fs::write(format!("{out}/census-entries.tsv"), tsv).expect("write");

    // ---- titles ----
    let mut tsv = String::from(
        "title\tclass\tfile\tcrc32\tentries\tentry_list\tbios_data_bytes\tbios_data_ranges\tram_boot_bytes\tram_live_bytes\tbios_time_pct\n",
    );
    for t in &titles {
        let p = &t.probe;
        let total = p.cycles_bios + p.cycles_other;
        let pct = if total == 0 { 0.0 } else { p.cycles_bios as f64 * 100.0 / total as f64 };
        let list: Vec<String> = p.entries.keys().map(|a| format!("{a:04X}")).collect();
        tsv.push_str(&format!(
            "{}\t{}\t{}\t{:08x}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\n",
            t.title,
            t.class,
            t.file,
            t.crc,
            p.entries.len(),
            list.join(","),
            p.data_reads.len(),
            ranges(p.data_reads.keys().copied()),
            p.ram_boot.len(),
            p.ram_live.len(),
            pct
        ));
    }
    std::fs::write(format!("{out}/census-titles.tsv"), tsv).expect("write");

    // ---- RAM contract ----
    #[derive(Default)]
    struct Ram {
        boot: usize,
        live: usize,
        values: BTreeSet<u8>,
    }
    let mut ram: BTreeMap<u16, Ram> = BTreeMap::new();
    for t in &titles {
        for &off in t.probe.ram_boot.keys() {
            let e = ram.entry(off).or_default();
            e.boot += 1;
            if let Some(h) = &t.probe.handover {
                e.values.insert(h[off as usize]);
            }
        }
        for &off in t.probe.ram_live.keys() {
            ram.entry(off).or_default().live += 1;
        }
    }
    let mut tsv = String::from("address\ttitles_boot\ttitles_live\thandover_values\n");
    for (off, r) in &ram {
        let vals: Vec<String> = r.values.iter().map(|v| format!("{v:02X}")).collect();
        tsv.push_str(&format!("{:04X}\t{}\t{}\t{}\n", 0x7000 + off, r.boot, r.live, vals.join(",")));
    }
    std::fs::write(format!("{out}/census-ram.tsv"), tsv).expect("write");

    // ---- data reads ----
    let mut data: BTreeMap<u16, Vec<&str>> = BTreeMap::new();
    for t in &titles {
        for &a in t.probe.data_reads.keys() {
            data.entry(a).or_default().push(&t.title);
        }
    }
    let mut tsv = String::from("address\ttitles\tsample_titles\n");
    for (a, ts) in &data {
        let sample: Vec<&str> = ts.iter().take(3).copied().collect();
        tsv.push_str(&format!("{:04X}\t{}\t{}\n", a, ts.len(), sample.join("; ")));
    }
    std::fs::write(format!("{out}/census-data-reads.tsv"), tsv).expect("write");

    // ---- returns into the BIOS ----
    // A return landing just after a CALL or RST in the real BIOS is the BIOS
    // resuming after a call-back into the cartridge: ordinary. Anything else
    // means the game returned into the BIOS through a stack word of its own.
    let resume = |a: u16| {
        let at = |o: u16| bios.get(a.wrapping_sub(o) as usize).copied().unwrap_or(0);
        let call = |op: u8| op == 0xcd || op & 0xc7 == 0xc4;
        let rst = |op: u8| op & 0xc7 == 0xc7;
        call(at(3)) || rst(at(1))
    };
    let mut rets: BTreeMap<u16, (usize, u64, Vec<String>)> = BTreeMap::new();
    for t in &titles {
        for (&a, e) in &t.probe.returns_into {
            let r = rets.entry(a).or_default();
            r.0 += 1;
            r.1 += e.calls;
            if r.2.len() < 3 {
                r.2.push(t.title.clone());
            }
        }
    }
    let mut tsv = String::from("address\tkind\ttitles\treturns\tsample_titles\n");
    for (a, (n, calls, ts)) in &rets {
        let kind = if resume(*a) { "resume" } else { "odd" };
        tsv.push_str(&format!("{a:04X}\t{kind}\t{n}\t{calls}\t{}\n", ts.join("; ")));
    }
    std::fs::write(format!("{out}/census-returns.tsv"), tsv).expect("write");
    let odd: Vec<String> = rets
        .iter()
        .filter(|(a, _)| !resume(**a))
        .map(|(a, (n, _, ts))| format!("{a:04X} ({n}: {})", ts.join("; ")))
        .collect();

    // ---- summary and coverage ----
    let commercial: Vec<&Title> = titles.iter().filter(|t| t.class == "commercial").collect();
    println!("{} titles ({} commercial) under {dir}", titles.len(), commercial.len());
    println!("{} distinct BIOS entry points used", entries.len());
    println!(
        "returns into the BIOS: {} addresses, {} not after a CALL/RST in the real BIOS: {}",
        rets.len(),
        odd.len(),
        odd.join(", ")
    );
    let no_calls = commercial.iter().filter(|t| t.probe.entries.is_empty()).count();
    println!("commercial titles calling nothing after boot: {no_calls}");
    let data_titles = commercial.iter().filter(|t| !t.probe.data_reads.is_empty()).count();
    println!("commercial titles reading BIOS bytes as data: {data_titles}");
    let boot_nonzero = ram.values().filter(|r| r.boot > 0 && r.values.iter().any(|&v| v != 0)).count();
    println!(
        "RAM read as the boot left it: {} bytes, {} of them non-zero at hand-over in some title",
        ram.values().filter(|r| r.boot > 0).count(),
        boot_nonzero
    );
    println!("RAM read as a BIOS routine left it during play: {} bytes", ram.values().filter(|r| r.live > 0).count());

    println!("\ncoverage of commercial titles by the N most-used entries:");
    let order: Vec<u16> = ranked.iter().map(|(a, _)| **a).collect();
    let mut have: BTreeSet<u16> = BTreeSet::new();
    let mut last = usize::MAX;
    for (n, a) in std::iter::once(None).chain(order.iter().map(Some)).enumerate() {
        if let Some(a) = a {
            have.insert(*a);
        }
        let covered = commercial.iter().filter(|t| t.probe.entries.keys().all(|e| have.contains(e))).count();
        if covered != last && (n <= 40 || covered == commercial.len()) {
            println!("  {n:>3} entries -> {covered:>3} / {} titles", commercial.len());
            last = covered;
        }
    }
    println!("\nledgers in {out}/census-*.tsv");
}
