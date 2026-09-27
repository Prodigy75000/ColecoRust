// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The BIOS census probe: what a cartridge asks of the BIOS, measured.
//!
//! This is the instrument the HLE is designed from. Run a title on the REAL
//! BIOS with a probe attached and it records:
//!
//! - **Entries**: every control transfer from cartridge or RAM code into the
//!   BIOS window, keyed by the address landed on. A return from cartridge code
//!   into the BIOS (the BIOS called the cartridge and it came back) is not an
//!   entry, and neither is the NMI, which the machine delivers, not the game.
//! - **Data reads**: BIOS bytes read by an instruction executing outside the
//!   BIOS. These are what a synthetic image would have to reproduce, and they
//!   decide between trapping at entry addresses and supplying an image.
//! - **The RAM contract**, in two parts, because they ask different things of
//!   the HLE. `ram_boot`: bytes the cartridge reads whose last writer was the
//!   BIOS's boot code, before the cartridge ever ran; with `handover` (RAM as
//!   the cartridge first sees it) this is "what RAM must hold at hand-over".
//!   `ram_live`: bytes written by BIOS routines while the game runs and then
//!   read by the game, which is state the HLE's routines must maintain in
//!   place. The first run lumped these together and reported 975 of 1024
//!   bytes, which was mostly the boot-time clear.
//! - **Cycles** spent executing in each region, once the cartridge has run.
//!
//! Nothing here is machine state: it is never saved, and a machine without a
//! probe pays one `Option` check per instruction and per memory access.

use crate::WORK_RAM;
use std::collections::BTreeMap;

/// Which code last wrote a RAM byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Writer {
    #[default]
    Nobody,
    /// BIOS code before the cartridge first ran.
    BiosBoot,
    /// BIOS code after it.
    Bios,
    Other,
}

#[derive(Default)]
pub struct Probe {
    /// PC of the instruction now executing.
    pub(crate) pc: u16,
    /// PC of the instruction before it, and its first opcode byte, so a
    /// transfer into the BIOS can be told apart from a return into it.
    prev_pc: u16,
    prev_op: [u8; 2],
    started: bool,
    /// Entry address to (calls, callers seen).
    pub entries: BTreeMap<u16, Entry>,
    /// BIOS address to times read as data from outside the BIOS.
    pub data_reads: BTreeMap<u16, u64>,
    last_writer: Vec<Writer>,
    /// RAM offset (0-1023) to reads by non-BIOS code of a byte whose last
    /// writer was the BIOS at boot, and during play.
    pub ram_boot: BTreeMap<u16, u64>,
    pub ram_live: BTreeMap<u16, u64>,
    /// RAM as it stood when cartridge code first executed.
    pub handover: Option<Vec<u8>>,
    pub cycles_bios: u64,
    pub cycles_other: u64,
    /// Set once cartridge code has executed; cycles are only counted after.
    pub cart_seen: bool,
}

#[derive(Default, Clone, Debug)]
pub struct Entry {
    pub calls: u64,
    /// Up to eight distinct call sites, for reading the code afterwards.
    pub callers: Vec<u16>,
}

fn in_bios(pc: u16) -> bool {
    pc < 0x2000
}

/// Opcodes that leave by popping a return address: RET, RET cc, and
/// RETI/RETN behind their ED prefix.
fn is_return(op: [u8; 2]) -> bool {
    op[0] == 0xc9 || op[0] & 0xc7 == 0xc0 || (op[0] == 0xed && (op[1] == 0x4d || op[1] == 0x45))
}

/// Instructions whose memory reads are stack pops: returns, POP, EX (SP),rr.
/// A game popping a return address the BIOS pushed (the BIOS called back into
/// the cartridge) is stack traffic, not a RAM contract; the first census run
/// counted it and put the whole stack at the top of the contract.
fn pops_stack(op: [u8; 2]) -> bool {
    let indexed = op[0] == 0xdd || op[0] == 0xfd;
    is_return(op)
        || op[0] & 0xcf == 0xc1
        || op[0] == 0xe3
        || (indexed && (op[1] == 0xe1 || op[1] == 0xe3))
}

impl Probe {
    pub fn new() -> Self {
        Probe { last_writer: vec![Writer::Nobody; WORK_RAM], ..Probe::default() }
    }

    /// Called before each instruction. `op` is its first two bytes, `nmi`
    /// whether the machine has just delivered an NMI to reach this PC.
    pub(crate) fn instruction(&mut self, pc: u16, op: [u8; 2], nmi: bool) {
        if pc >= 0x8000 {
            self.cart_seen = true;
        }
        if self.started && in_bios(pc) && !in_bios(self.prev_pc) && !nmi && !is_return(self.prev_op) {
            let e = self.entries.entry(pc).or_default();
            e.calls += 1;
            if e.callers.len() < 8 && !e.callers.contains(&self.prev_pc) {
                e.callers.push(self.prev_pc);
            }
        }
        self.started = true;
        self.prev_pc = pc;
        self.prev_op = op;
        self.pc = pc;
    }

    pub(crate) fn cycles(&mut self, pc: u16, c: i32) {
        if !self.cart_seen {
            return;
        }
        if in_bios(pc) {
            self.cycles_bios += c as u64;
        } else {
            self.cycles_other += c as u64;
        }
    }

    pub(crate) fn read(&mut self, addr: u16) {
        if in_bios(self.pc) {
            return;
        }
        if in_bios(addr) {
            *self.data_reads.entry(addr).or_default() += 1;
        } else if (0x6000..0x8000).contains(&addr) && !pops_stack(self.prev_op) {
            // `prev_op` is the current instruction's: `instruction` has
            // already run for it.
            let off = addr as usize & (WORK_RAM - 1);
            let map = match self.last_writer[off] {
                Writer::BiosBoot => &mut self.ram_boot,
                Writer::Bios => &mut self.ram_live,
                _ => return,
            };
            *map.entry(off as u16).or_default() += 1;
        }
    }

    pub(crate) fn write(&mut self, addr: u16) {
        if (0x6000..0x8000).contains(&addr) {
            let off = addr as usize & (WORK_RAM - 1);
            self.last_writer[off] = match (in_bios(self.pc), self.cart_seen) {
                (true, false) => Writer::BiosBoot,
                (true, true) => Writer::Bios,
                _ => Writer::Other,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_into_the_bios_is_an_entry_and_a_return_into_it_is_not() {
        let mut p = Probe::new();
        p.instruction(0x8100, [0xcd, 0x00], false); // CALL from the cartridge
        p.instruction(0x1f61, [0xc3, 0x00], false); // lands in the BIOS
        p.instruction(0x8200, [0xc9, 0x00], false); // BIOS called the cart; it RETs
        p.instruction(0x0500, [0x00, 0x00], false); // back in the BIOS: not an entry
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[&0x1f61].calls, 1);
        assert_eq!(p.entries[&0x1f61].callers, vec![0x8100]);
    }

    #[test]
    fn the_nmi_is_not_an_entry() {
        let mut p = Probe::new();
        p.instruction(0x8100, [0x00, 0x00], false);
        p.instruction(0x0066, [0xf5, 0x00], true);
        assert!(p.entries.is_empty());
    }

    #[test]
    fn a_ram_byte_counts_only_if_the_bios_wrote_it_last() {
        let mut p = Probe::new();
        p.pc = 0x0100;
        p.write(0x7020); // BIOS writes $7020 at boot
        p.write(0x7021);
        p.instruction(0x8000, [0, 0], false); // the cartridge starts
        p.write(0x6021); // and overwrites $7021 through a mirror
        p.read(0x7020);
        p.read(0x7021);
        assert_eq!(p.ram_boot.get(&0x20), Some(&1));
        assert_eq!(p.ram_boot.get(&0x21), None);
        assert!(p.ram_live.is_empty());
    }

    /// The cartridge's RET popping a return address the BIOS pushed is stack
    /// traffic; the same byte read by a plain load is a contract read.
    #[test]
    fn popping_what_the_bios_pushed_is_not_a_contract_read() {
        let mut p = Probe::new();
        p.instruction(0x8000, [0, 0], false);
        p.instruction(0x1f61, [0, 0], false);
        p.write(0x73b0); // a BIOS CALL pushes a return address
        p.instruction(0x8100, [0xc9, 0], false); // the cartridge routine RETs
        p.read(0x73b0);
        assert!(p.ram_live.is_empty());
        p.instruction(0x8101, [0x3a, 0], false); // LD A,($73B0)
        p.read(0x73b0);
        assert_eq!(p.ram_live.get(&0x3b0), Some(&1));
    }

    #[test]
    fn a_bios_write_during_play_is_live_state() {
        let mut p = Probe::new();
        p.instruction(0x8000, [0xcd, 0], false);
        p.instruction(0x1f61, [0, 0], false); // a BIOS routine
        p.write(0x7030);
        p.instruction(0x8003, [0, 0], false);
        p.read(0x7030);
        assert_eq!(p.ram_live.get(&0x30), Some(&1));
        assert!(p.ram_boot.is_empty());
    }

    #[test]
    fn bios_bytes_read_by_the_cartridge_are_data_reads() {
        let mut p = Probe::new();
        p.pc = 0x0100;
        p.read(0x0200); // the BIOS reading itself: not counted
        p.pc = 0x8000;
        p.read(0x006b);
        assert_eq!(p.data_reads.len(), 1);
        assert_eq!(p.data_reads[&0x006b], 1);
    }
}
