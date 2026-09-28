// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The serial EEPROM on Activision-style cartridges: a 24C-series part on a
//! two-wire (I2C) bus, which is how Black Onyx and Boxxle keep saved games.
//!
//! **Sources.** The protocol is the 24C-series datasheet's, as generic I2C:
//! data (SDA) changes while the clock (SCL) is low and is sampled on the
//! clock's rising edge; SDA falling while SCL is high is a START, rising
//! while SCL is high a STOP; after every eighth bit the receiver pulls SDA low
//! for one clock to acknowledge. Which part each game carries (Black Onyx a
//! 24C08, Boxxle a 24C256) is from ColEm's documentation and ColecoDS, which
//! both choose it per game; the cartridge gives no way to tell.
//!
//! The two sizes are addressed differently, which is why the choice matters:
//!
//! - a **24C08** (1 KB) takes a one-byte word address, and the top two
//!   address bits ride in the device-select byte (`1010 x P1 P0 R/W`);
//! - a **24C256** (32 KB) takes a two-byte word address, high byte first.
//!
//! Writes land at once. The part's few milliseconds of write time are not
//! modelled: a game that polls for the acknowledge gets it on the first try.

use crate::save::{LoadError, ReadCursor, SaveState, WriteCursor};

/// The parts the Activision cartridges carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    C24C08,
    C24C256,
}

impl Chip {
    pub fn size(self) -> usize {
        match self {
            Chip::C24C08 => 1024,
            Chip::C24C256 => 32 * 1024,
        }
    }

    /// Bytes a write can fill before the address wraps within its page.
    fn page(self) -> usize {
        match self {
            Chip::C24C08 => 16,
            Chip::C24C256 => 64,
        }
    }

    fn wide_address(self) -> bool {
        self == Chip::C24C256
    }
}

/// What the part is doing with the byte on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for a START.
    Idle,
    /// Receiving the device-select byte.
    Select,
    /// Receiving the high byte of the word address (24C256 only).
    AddressHigh,
    /// Receiving the low byte of the word address.
    AddressLow,
    /// Receiving data to write.
    Data,
    /// Sending data to the master.
    Send,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eeprom {
    pub chip: Chip,
    /// The contents: what a frontend keeps as the game's save file.
    pub data: Vec<u8>,
    phase: Phase,
    /// Bits of the current byte clocked so far; 8 is the acknowledge clock.
    bit: u8,
    shift: u8,
    /// The word address, the part's internal counter.
    address: u16,
    /// The master's lines, as last written.
    scl: bool,
    sda: bool,
    /// What the part drives onto SDA: false pulls it low.
    out: bool,
    /// The part's own acknowledge clock is under way.
    acking: bool,
}

impl Eeprom {
    /// A blank part: erased, every byte `$FF`.
    pub fn new(chip: Chip) -> Self {
        Eeprom {
            chip,
            data: vec![0xff; chip.size()],
            phase: Phase::Idle,
            bit: 0,
            shift: 0,
            address: 0,
            scl: true,
            sda: true,
            out: true,
            acking: false,
        }
    }

    /// The SDA line as the CPU reads it: low if either side pulls it low.
    pub fn read_sda(&self) -> bool {
        self.sda && self.out
    }

    pub fn set_scl(&mut self, high: bool) {
        let was = self.scl;
        self.scl = high;
        if !was && high {
            self.rising();
        } else if was && !high {
            self.falling();
        }
    }

    pub fn set_sda(&mut self, high: bool) {
        let was = self.sda;
        self.sda = high;
        if self.scl && was != high {
            if high {
                // STOP.
                self.phase = Phase::Idle;
                self.out = true;
            } else {
                // START, or a repeated START: the address counter is kept, which
                // is what makes a random read (write the address, START again,
                // read) work.
                self.phase = Phase::Select;
                self.bit = 0;
                self.shift = 0;
                self.out = true;
                self.acking = false;
            }
        }
    }

    fn mask(&self) -> u16 {
        (self.chip.size() - 1) as u16
    }

    fn rising(&mut self) {
        if self.acking {
            return;
        }
        match self.phase {
            Phase::Idle => {}
            Phase::Send => {
                if self.bit < 8 {
                    self.bit += 1;
                } else if self.sda {
                    // No acknowledge from the master: it wants no more.
                    self.phase = Phase::Idle;
                } else {
                    self.bit = 9;
                }
            }
            _ => {
                if self.bit < 8 {
                    self.shift = (self.shift << 1) | self.sda as u8;
                    self.bit += 1;
                }
            }
        }
    }

    fn falling(&mut self) {
        if self.acking {
            // The acknowledge clock is over.
            self.acking = false;
            self.out = true;
            self.bit = 0;
            self.shift = 0;
            if self.phase == Phase::Send {
                self.send_next();
            }
            return;
        }
        match self.phase {
            Phase::Idle => self.out = true,
            Phase::Send => {
                if self.bit == 9 {
                    self.send_next();
                } else if self.bit < 8 {
                    self.out = self.shift & (0x80 >> self.bit) != 0;
                } else {
                    // Let go of SDA for the master's acknowledge.
                    self.out = true;
                }
            }
            _ if self.bit == 8 => self.byte_received(),
            _ => {}
        }
    }

    /// Start sending the byte at the address counter, and count past it.
    fn send_next(&mut self) {
        self.shift = self.data[self.address as usize];
        self.address = (self.address + 1) & self.mask();
        self.bit = 0;
        self.out = self.shift & 0x80 != 0;
    }

    fn byte_received(&mut self) {
        let b = self.shift;
        let next = match self.phase {
            Phase::Select => {
                if b & 0xf0 != 0xa0 {
                    // Not addressed to a 24C part: stay off the bus.
                    self.phase = Phase::Idle;
                    return;
                }
                if !self.chip.wide_address() {
                    // The block bits, P1 P0 on a 24C08.
                    let block = ((b as u16 >> 1) & 7) << 8;
                    self.address = (block | (self.address & 0xff)) & self.mask();
                }
                if b & 1 != 0 {
                    Phase::Send
                } else if self.chip.wide_address() {
                    Phase::AddressHigh
                } else {
                    Phase::AddressLow
                }
            }
            Phase::AddressHigh => {
                self.address = ((b as u16) << 8) & self.mask();
                Phase::AddressLow
            }
            Phase::AddressLow => {
                self.address = ((self.address & !0xff) | b as u16) & self.mask();
                Phase::Data
            }
            Phase::Data => {
                self.data[self.address as usize] = b;
                let page = self.chip.page() as u16;
                self.address = (self.address & !(page - 1)) | ((self.address + 1) & (page - 1));
                Phase::Data
            }
            Phase::Idle | Phase::Send => return,
        };
        self.phase = next;
        // Acknowledge: pull SDA low for the ninth clock.
        self.out = false;
        self.acking = true;
    }
}

impl SaveState for Eeprom {
    fn save(&self, w: &mut WriteCursor) {
        w.bytes(&self.data);
        w.u8(match self.phase {
            Phase::Idle => 0,
            Phase::Select => 1,
            Phase::AddressHigh => 2,
            Phase::AddressLow => 3,
            Phase::Data => 4,
            Phase::Send => 5,
        });
        w.u8(self.bit);
        w.u8(self.shift);
        w.u16(self.address);
        w.bool(self.scl);
        w.bool(self.sda);
        w.bool(self.out);
        w.bool(self.acking);
    }

    fn load(&mut self, r: &mut ReadCursor) -> Result<(), LoadError> {
        r.bytes(&mut self.data)?;
        self.phase = match r.u8()? {
            0 => Phase::Idle,
            1 => Phase::Select,
            2 => Phase::AddressHigh,
            3 => Phase::AddressLow,
            4 => Phase::Data,
            5 => Phase::Send,
            _ => return Err(LoadError::BadValue("EEPROM phase")),
        };
        self.bit = r.u8()?;
        if self.bit > 9 {
            return Err(LoadError::BadValue("EEPROM bit count"));
        }
        self.shift = r.u8()?;
        self.address = r.u16()? & self.mask();
        self.scl = r.bool()?;
        self.sda = r.bool()?;
        self.out = r.bool()?;
        self.acking = r.bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bit-banging master, driving the lines the way a game does.
    struct Master<'a>(&'a mut Eeprom);

    impl Master<'_> {
        fn start(&mut self) {
            self.0.set_sda(true);
            self.0.set_scl(true);
            self.0.set_sda(false);
            self.0.set_scl(false);
        }
        fn stop(&mut self) {
            self.0.set_sda(false);
            self.0.set_scl(true);
            self.0.set_sda(true);
        }
        /// Send a byte; true if the part acknowledged it.
        fn put(&mut self, b: u8) -> bool {
            for i in (0..8).rev() {
                self.0.set_sda(b >> i & 1 != 0);
                self.0.set_scl(true);
                self.0.set_scl(false);
            }
            self.0.set_sda(true);
            self.0.set_scl(true);
            let ack = !self.0.read_sda();
            self.0.set_scl(false);
            ack
        }
        /// Receive a byte, then acknowledge it (for more) or not.
        fn get(&mut self, more: bool) -> u8 {
            self.0.set_sda(true);
            let mut b = 0;
            for _ in 0..8 {
                self.0.set_scl(true);
                b = b << 1 | self.0.read_sda() as u8;
                self.0.set_scl(false);
            }
            self.0.set_sda(!more);
            self.0.set_scl(true);
            self.0.set_scl(false);
            b
        }
    }

    #[test]
    fn a_24c256_writes_and_reads_back_with_two_byte_addresses() {
        let mut e = Eeprom::new(Chip::C24C256);
        let mut m = Master(&mut e);
        m.start();
        assert!(m.put(0xa0), "device select acknowledged");
        assert!(m.put(0x12));
        assert!(m.put(0x34));
        for b in [0xde, 0xad, 0xbe, 0xef] {
            assert!(m.put(b));
        }
        m.stop();
        assert_eq!(&e.data[0x1234..0x1238], &[0xde, 0xad, 0xbe, 0xef]);

        // Random read: set the address with a write, START again, read.
        let mut m = Master(&mut e);
        m.start();
        m.put(0xa0);
        m.put(0x12);
        m.put(0x35);
        m.start();
        assert!(m.put(0xa1));
        let got = [m.get(true), m.get(true), m.get(false)];
        m.stop();
        assert_eq!(got, [0xad, 0xbe, 0xef]);
    }

    /// A 24C08 takes one address byte and the block in the select byte: a
    /// 24C256's second address byte would be written as data here.
    #[test]
    fn a_24c08_takes_its_block_from_the_select_byte() {
        let mut e = Eeprom::new(Chip::C24C08);
        let mut m = Master(&mut e);
        m.start();
        assert!(m.put(0xa0 | 2 << 1)); // block 2
        assert!(m.put(0x10));
        assert!(m.put(0x77));
        assert!(m.put(0x88));
        m.stop();
        assert_eq!(&e.data[0x210..0x212], &[0x77, 0x88]);

        let mut m = Master(&mut e);
        m.start();
        m.put(0xa4);
        m.put(0x11);
        m.start();
        m.put(0xa5);
        assert_eq!(m.get(false), 0x88);
        m.stop();
    }

    #[test]
    fn a_page_write_wraps_within_its_page() {
        let mut e = Eeprom::new(Chip::C24C08);
        let mut m = Master(&mut e);
        m.start();
        m.put(0xa0);
        m.put(0x0e); // two bytes before the end of a 16-byte page
        for b in [1, 2, 3, 4] {
            m.put(b);
        }
        m.stop();
        assert_eq!(&e.data[0x0e..0x10], &[1, 2]);
        assert_eq!(&e.data[0x00..0x02], &[3, 4], "wrapped to the page start");
        assert_eq!(e.data[0x10], 0xff, "not into the next page");
    }

    #[test]
    fn another_device_address_gets_no_acknowledge() {
        let mut e = Eeprom::new(Chip::C24C256);
        let mut m = Master(&mut e);
        m.start();
        assert!(!m.put(0x90));
        m.stop();
        assert!(e.data.iter().all(|&b| b == 0xff));
    }

    #[test]
    fn state_round_trips() {
        let mut e = Eeprom::new(Chip::C24C08);
        let mut m = Master(&mut e);
        m.start();
        m.put(0xa0);
        m.put(0x05);
        m.put(0x42);
        let mut w = WriteCursor::new();
        e.save(&mut w);
        let bytes = w.into_bytes();
        let mut f = Eeprom::new(Chip::C24C08);
        let mut r = ReadCursor::new(&bytes);
        f.load(&mut r).unwrap();
        r.finish().unwrap();
        assert_eq!(e, f);
    }
}
