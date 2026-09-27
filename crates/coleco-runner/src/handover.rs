// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! `handover`: the machine as a BIOS leaves it at the cartridge's first
//! instruction, for every title, and what is common to all of them.
//!
//!   handover --bios bios/coleco.rom dumps/corpus
//!
//! This is the specification of the HLE's boot: whatever is identical across
//! every title is what the HLE must reproduce; whatever varies is either
//! cartridge-derived (the title screen's text) or does not matter. Split by
//! the header's first two bytes, because `$AA55` goes through the title
//! screen and `$55AA` does not, and they may leave different machines.

use coleco_core::machine::Coleco;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[path = "corpus.rs"]
mod corpus;
#[path = "png.rs"]
mod png;

/// Named fields of the hand-over state, as `(name, value)`.
fn snapshot(m: &Coleco) -> Vec<(String, String)> {
    let c = &m.cpu;
    let mut v = vec![
        ("pc".into(), format!("{:04X}", c.pc)),
        ("sp".into(), format!("{:04X}", c.sp)),
        ("af".into(), format!("{:02X}{:02X}", c.a(), c.f)),
        ("bc".into(), format!("{:04X}", c.bc())),
        ("de".into(), format!("{:04X}", c.de())),
        ("hl".into(), format!("{:04X}", c.hl())),
        ("ix".into(), format!("{:04X}", c.ix)),
        ("iy".into(), format!("{:04X}", c.iy)),
        ("i".into(), format!("{:02X}", c.i)),
        ("im".into(), format!("{}", c.im)),
        ("iff1".into(), format!("{}", c.iff1)),
    ];
    for (r, val) in m.bus.vdp.regs.iter().enumerate() {
        v.push((format!("vdp r{r}"), format!("{val:02X}")));
    }
    let vram = &m.bus.vdp.vram[..];
    v.push(("vram crc".into(), format!("{:08X}", png::crc32(vram))));
    v.push((
        "vram nonzero".into(),
        format!("{}", vram.iter().filter(|&&b| b != 0).count()),
    ));
    for (i, &b) in m.bus.ram.iter().enumerate() {
        v.push((format!("ram {:04X}", 0x7000 + i), format!("{b:02X}")));
    }
    v
}

fn main() {
    let mut bios_path: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bios" => bios_path = args.next(),
            _ => dir = Some(a),
        }
    }
    let (Some(bios_path), Some(dir)) = (bios_path, dir) else {
        eprintln!("usage: handover --bios PATH CARTDIR");
        std::process::exit(2);
    };
    let bios = std::fs::read(&bios_path).expect("read bios");
    let root = PathBuf::from(&dir);

    // header kind -> field -> value -> titles
    let mut seen: BTreeMap<&str, BTreeMap<String, BTreeMap<String, usize>>> = BTreeMap::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in corpus::cartridges(&root) {
        let file = p
            .strip_prefix(&root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        let (_, dump, _) = corpus::classify(&file);
        if dump == "bad" {
            continue;
        }
        let cart = std::fs::read(&p).expect("read cart");
        let kind = corpus::header(&cart);
        let Ok(mut m) = Coleco::new(Coleco::bios_from_bytes(&bios).unwrap(), &cart) else {
            continue;
        };
        // Up to 30 s: past the title delay with margin.
        let mut steps = 0u64;
        while m.cpu.pc < 0x8000 && steps < 40_000_000 {
            m.step();
            steps += 1;
        }
        if m.cpu.pc < 0x8000 {
            continue;
        }
        *counts.entry(kind).or_default() += 1;
        let by_field = seen.entry(kind).or_default();
        for (k, v) in snapshot(&m) {
            *by_field.entry(k).or_default().entry(v).or_default() += 1;
        }
    }

    for (kind, fields) in &seen {
        let n = counts[kind];
        println!("== header {kind}: {n} dumps reached the cartridge ==");
        let mut ram_same_nonzero = Vec::new();
        let mut ram_varies = Vec::new();
        for (field, values) in fields {
            let is_ram = field.starts_with("ram ");
            if values.len() == 1 {
                let (v, _) = values.iter().next().unwrap();
                if is_ram {
                    if v != "00" {
                        ram_same_nonzero.push(format!("{}={v}", &field[4..]));
                    }
                } else {
                    println!("  {field:<13} always {v}");
                }
            } else if is_ram {
                let vs: Vec<String> = values
                    .iter()
                    .take(6)
                    .map(|(v, c)| format!("{v}x{c}"))
                    .collect();
                ram_varies.push(format!("{}[{}]", &field[4..], vs.join(" ")));
            } else {
                let mut vs: Vec<(&String, &usize)> = values.iter().collect();
                vs.sort_by(|a, b| b.1.cmp(a.1));
                let shown: Vec<String> = vs
                    .iter()
                    .take(4)
                    .map(|(v, c)| format!("{v} x{c}"))
                    .collect();
                println!(
                    "  {field:<13} VARIES ({} values): {}",
                    values.len(),
                    shown.join(", ")
                );
            }
        }
        println!(
            "  RAM identical and non-zero in every title: {}",
            ram_same_nonzero.join(" ")
        );
        println!(
            "  RAM varying between titles: {} bytes: {}",
            ram_varies.len(),
            ram_varies.join(" ")
        );
    }
}
