// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! ZEXALL / ZEXDOC conformance harness for the Z80.
//!
//! Lifted from MegaRust's `md-runner` along with the CPU itself (`origin/main`
//! 3e97ca7). The ROMs are not in git (`/tests/` is ignored here); copy them from
//! MegaRust's `tests/z80-zexall/`, whose `SOURCES.md` names the upstream release.
//!
//! ZEXALL runs every instruction over an exhaustive set of operands, CRCs the
//! results, and compares against values captured from real hardware. The CPU
//! is ground to green here before it is trusted to drive anything.
//!
//! The Master System build is fine for a ColecoVision core: it runs on its own
//! test bus below, which is not either machine, and what it measures is the
//! Z80.
//!
//! The vendored build is the Sega Master System port (v0.21), so this stands up
//! just enough SMS to run it: 64 KiB of Z80 address space, the ROM paged in
//! through the Sega mapper, 8 KiB of work RAM, and cartridge RAM. **No VDP.**
//! That build emits its results to the SDSC debug console as well as the
//! screen, and the console is two I/O ports, so the whole output path is a
//! `match` on a port number rather than a display.
//!
//!   cargo run --release -p coleco-runner --bin zexall -- tests/z80-zexall/zexdoc.sms
//!   cargo run --release -p coleco-runner --bin zexall -- --max-cycles 40000000000 tests/z80-zexall/zexall.sms
//!
//! `zexdoc` masks the undocumented flag bits and is the first gate; `zexall`
//! includes them and is the harder one. Both take a while: the longest single
//! test is minutes of emulated time.

use coleco_core::z80::{Bus, Z80};

/// The SDSC debug console, a two-port convention SMS emulators implement so a
/// ROM can print before its VDP works. The vendored build does not actually use
/// it (it wants a detection handshake first), so the report is read out of
/// cartridge RAM instead; these stay because another build might.
const PORT_SDSC_CONTROL: u8 = 0xfd;
const PORT_SDSC_DATA: u8 = 0xfc;

/// Tests in the exerciser's table, counted from `source/zexall.sms.asm` in the
/// upstream release. Both ZEXDOC and ZEXALL run the same 79; they differ in
/// whether the undocumented flag bits are masked out of the comparison.
///
/// Knowing the total is what makes a run *finished* rather than merely long: the
/// ROM loops forever once it is done, so without this the harness could only
/// ever report "cycle limit reached".
const TOTAL_TESTS: usize = 79;

/// Just enough Master System to run a self-contained test ROM.
struct SmsBus {
    rom: Vec<u8>,
    /// 8 KiB at $C000, mirrored to the top of the space.
    ram: [u8; 0x2000],
    /// 32 KiB of cartridge RAM; v0.21 also writes its output here.
    cart_ram: [u8; 0x8000],
    /// The Sega mapper's three 16 KiB slots and its control byte.
    banks: [usize; 3],
    ram_control: u8,
    /// Everything the ROM printed through the SDSC port, when it uses it.
    output: String,
    /// Set when the ROM asks the console to terminate.
    finished: bool,
}

impl SmsBus {
    /// The ROM's other output path: it writes its report to cartridge RAM as
    /// plain ASCII, terminated by the first zero. v0.21 does this whatever else
    /// it is driving, which makes it the one output that needs no detection
    /// handshake and no display.
    fn sram_text(&self) -> String {
        self.cart_ram
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| b as char)
            .collect()
    }

    fn new(rom: Vec<u8>) -> Self {
        SmsBus {
            rom,
            ram: [0; 0x2000],
            cart_ram: [0; 0x8000],
            // Power-on mapping is the first three banks.
            banks: [0, 1, 2],
            ram_control: 0,
            output: String::new(),
            finished: false,
        }
    }

    fn rom_byte(&self, bank: usize, offset: usize) -> u8 {
        let i = bank * 0x4000 + offset;
        if i < self.rom.len() {
            self.rom[i]
        } else {
            0xff
        }
    }
}

impl Bus for SmsBus {
    fn read(&mut self, addr: u16) -> u8 {
        let a = addr as usize;
        match a {
            // The first KiB is always bank 0, so the reset and interrupt vectors
            // cannot be paged away.
            0x0000..=0x03ff => self.rom_byte(0, a),
            0x0400..=0x3fff => self.rom_byte(self.banks[0], a),
            0x4000..=0x7fff => self.rom_byte(self.banks[1], a - 0x4000),
            0x8000..=0xbfff => {
                if self.ram_control & 0x08 != 0 {
                    // Cartridge RAM paged into the third slot.
                    let half = if self.ram_control & 0x04 != 0 { 0x4000 } else { 0 };
                    self.cart_ram[half + (a - 0x8000)]
                } else {
                    self.rom_byte(self.banks[2], a - 0x8000)
                }
            }
            // Work RAM, mirrored once across the top 16 KiB.
            _ => self.ram[(a - 0xc000) & 0x1fff],
        }
    }

    fn write(&mut self, addr: u16, val: u8) {
        let a = addr as usize;
        match a {
            0x8000..=0xbfff => {
                if self.ram_control & 0x08 != 0 {
                    let half = if self.ram_control & 0x04 != 0 { 0x4000 } else { 0 };
                    self.cart_ram[half + (a - 0x8000)] = val;
                }
            }
            0xc000..=0xffff => {
                self.ram[(a - 0xc000) & 0x1fff] = val;
                // The mapper registers live in the last four bytes of the space
                // and are written through RAM, so both effects happen.
                match a {
                    0xfffc => self.ram_control = val,
                    0xfffd => self.banks[0] = val as usize,
                    0xfffe => self.banks[1] = val as usize,
                    0xffff => self.banks[2] = val as usize,
                    _ => {}
                }
            }
            _ => {} // ROM
        }
    }

    fn input(&mut self, port: u16) -> u8 {
        match port as u8 {
            // The controller ports read as "nothing pressed", which is what the
            // ROM's "press Up to force mode 4" check needs to see.
            0xdc | 0xc0 => 0xff,
            0xdd | 0xc1 => 0xff,
            // A VDP status read. Reporting the vblank flag set keeps the ROM's
            // wait loops moving without a VDP behind them.
            0xbe => 0x00,
            0xbf | 0x7e | 0x7f => 0x80,
            _ => 0xff,
        }
    }

    fn output(&mut self, port: u16, val: u8) {
        match port as u8 {
            PORT_SDSC_DATA => {
                let c = val as char;
                // The ROM ends lines with CR LF and pads with nulls.
                if c == '\n' || c == '\r' || (' '..='~').contains(&c) {
                    self.output.push(c);
                }
            }
            PORT_SDSC_CONTROL => {
                // Control 0 asks the console to close, which is how the ROM says
                // it is done.
                if val == 0 {
                    self.finished = true;
                }
            }
            _ => {}
        }
    }
}

/// Failed tests in the report. v0.21 does not print the word ERROR: a failing
/// test is its name followed by `CRC xxxxxxxx expected yyyyyyyy`. Counting
/// "ERROR", as this harness first did, scored every failure as missing, so a
/// broken CPU ran to the cycle cap and was reported INCOMPLETE rather than
/// failed. Measured 2026-09-27 with a deliberately broken CP: 14 mismatches,
/// 0 "ERROR".
fn tests_failed(text: &str) -> usize {
    text.matches(" expected ").count() + text.matches("ERROR").count()
}

/// Tests the report has reached a verdict on. The ROM prints one line per test,
/// ending in `OK` or naming the CRC mismatch, then `Tests complete`.
fn tests_reported(text: &str) -> usize {
    text.matches("OK").count() + tests_failed(text)
}

/// The ROM says when it is done, whatever the verdicts were.
fn report_finished(text: &str) -> bool {
    text.contains("Tests complete") || tests_reported(text) >= TOTAL_TESTS
}

/// Print whatever whole lines have appeared since `printed`, and return the new
/// mark. The report arrives CR-terminated, so it is re-emitted with newlines: a
/// log full of bare CRs reads as a single line overwriting itself, which is why
/// the first runs of this harness looked silent when they were not.
fn print_new_lines(text: &str, printed: usize) -> usize {
    let from = printed.min(text.len());
    let Some(end) = text[from..].rfind(['\n', '\r']) else {
        return printed;
    };
    for line in text[from..from + end + 1].split(['\n', '\r']) {
        if !line.trim().is_empty() {
            println!("  {line}");
        }
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
    from + end + 1
}

fn main() {
    let mut max_cycles: i64 = 20_000_000_000;
    let mut paths: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--max-cycles" => {
                max_cycles = args.next().and_then(|v| v.parse().ok()).unwrap_or(max_cycles)
            }
            other => paths.push(other.to_string()),
        }
    }
    if paths.is_empty() {
        eprintln!(
            "usage: zexall [--max-cycles N] <zexdoc.sms|zexall.sms>\n\
             Runs the Z80 exerciser and prints its results."
        );
        std::process::exit(2);
    }

    let mut failed = 0;
    for path in &paths {
        let rom = match std::fs::read(path) {
            Ok(r) => r,
            Err(e) => {
                println!("{path}: cannot read: {e}");
                failed += 1;
                continue;
            }
        };
        println!("== {path} ==");
        let mut bus = SmsBus::new(rom);
        let mut cpu = Z80::new();
        cpu.reset();
        cpu.sp = 0xdff0;

        let mut cycles: i64 = 0;
        let mut printed = 0usize;
        let mut steps: u64 = 0;
        let mut complete = false;
        while cycles < max_cycles && !bus.finished && !complete {
            cycles += cpu.step(&mut bus) as i64;
            steps += 1;
            // Stream the report as it grows, and stop as soon as every test has
            // a verdict. Checking now and then rather than every instruction
            // keeps the scan off the hot path; without the check the run would
            // burn its whole budget sitting on a finished, green result,
            // because the ROM loops forever once it is done.
            if steps % 200_000 == 0 {
                let text = bus.sram_text();
                printed = print_new_lines(&text, printed);
                complete = report_finished(&text);
            }
        }
        let final_text = bus.sram_text();
        print_new_lines(&final_text, printed);

        if !complete {
            // Where it stopped, and whether the ROM's other output path (ASCII
            // into cartridge RAM) has anything, which together say whether the
            // CPU is wedged or the harness is not being talked to.
            println!(
                "  stopped at PC ${:04X}  AF {:02X}{:02X} BC {:04X} DE {:04X} HL {:04X} SP {:04X}",
                cpu.pc, cpu.a(), cpu.f, cpu.bc(), cpu.de(), cpu.hl(), cpu.sp
            );
            println!(
                "  banks {:?} ram_control ${:02X}  cart-ram printable {} bytes",
                bus.banks,
                bus.ram_control,
                bus.cart_ram.iter().filter(|&&b| (0x20..0x7f).contains(&b)).count()
            );
            let ascii: String = bus
                .cart_ram
                .iter()
                .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                .collect();
            let trimmed = ascii.trim_matches('.');
            if !trimmed.is_empty() {
                println!("  cart ram: {}", &trimmed[..trimmed.len().min(300)]);
            }
        }

        let text = if bus.output.is_empty() { bus.sram_text() } else { bus.output.clone() };
        let errors = tests_failed(&text);
        let oks = text.matches("OK").count();
        // Complete means every test has a verdict. "Tests complete" alone is
        // not enough: a ROM that skipped tests would say it too.
        let complete = oks + errors >= TOTAL_TESTS;
        println!(
            "-- {oks}/{TOTAL_TESTS} OK, {errors} errors, {cycles} cycles: {} --",
            if complete && errors == 0 {
                "ALL GREEN"
            } else if complete {
                "COMPLETE WITH FAILURES"
            } else {
                "INCOMPLETE, raise --max-cycles"
            }
        );
        if errors > 0 || !complete {
            failed += 1;
        }
    }

    if failed > 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim tail of a real v0.21 report from a CPU with a broken CP
    /// (2026-09-27). The harness first counted "ERROR" and scored this 0 failures.
    const FAILING_TAIL: &str = "  daa OK
  adc/sbc hl,rp OK
  aluop a,(ix/y+1) 
   CRC e542ab44 expected 6c506ef4
  Tests complete
";

    #[test]
    fn a_crc_mismatch_counts_as_a_failure() {
        assert_eq!(tests_failed(FAILING_TAIL), 1);
        assert_eq!(tests_reported(FAILING_TAIL), 3);
    }

    #[test]
    fn the_rom_saying_it_is_done_ends_the_run_even_with_failures() {
        assert!(report_finished(FAILING_TAIL));
        assert!(!report_finished("  daa OK
  neg OK
"));
    }
}
