// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The ColecoVision: memory map, I/O ports, controllers, and the frame loop.
//!
//! **Sources.** None of this is in our own notes yet; it is the published
//! ColecoVision memory and port map as every emulator and the homebrew
//! community describe it, written here from that common description and not
//! from any one document. It needs a derived write-up in `docs/ref/` with
//! citations, and until then every claim below is checkable against the real
//! BIOS, which is the point of running it: a wrong port decode shows up as
//! the BIOS title screen not appearing.
//!
//! | Range | What |
//! |---|---|
//! | `$0000-$1FFF` | BIOS, 8 KB |
//! | `$2000-$5FFF` | expansion port, open bus here |
//! | `$6000-$7FFF` | 1 KB of RAM, mirrored eight times |
//! | `$8000-$FFFF` | cartridge, up to 32 KB |
//!
//! Ports are decoded on address bits 7-5 only, so each function owns 32 ports:
//! `$80` writes select keypad mode, `$A0` is the VDP (bit 0: data or
//! control), `$C0` writes select joystick mode, `$E0` writes go to the PSG and
//! reads return a controller (bit 1: which one).
//!
//! The VDP's interrupt output is wired to the Z80's **NMI**, not its INT, and
//! NMI is edge-triggered. So an interrupt is taken when the VDP line goes
//! active, once, and a game that enables interrupts while F is already set
//! gets one at that moment. This is the single wiring fact most likely to be
//! gotten subtly wrong.

use crate::census::Probe;
use crate::psg::Audio;
use crate::save::{LoadError, ReadCursor, SaveState, WriteCursor};
use crate::vdp::{self, Vdp};
use crate::z80::{Bus, Z80};
use crate::{BIOS_SIZE, WORK_RAM};

/// Z80 clocks per scanline: 342 VDP pixel clocks at two-thirds of a CPU
/// clock each.
pub const CYCLES_PER_LINE: i32 = 228;

/// Leading magic on a ColecoRust state, so it can never be fed to another core.
pub const STATE_MAGIC: &[u8; 8] = b"COLECO01";

/// Largest cartridge the plain map addresses. Bank-switched boards come later.
pub const MAX_CART: usize = 0x8000;

/// The firmware the machine runs from.
pub enum Firmware {
    /// The reimplemented BIOS: our own image, with traps into host code.
    /// See [`crate::hle`].
    Hle,
    /// A real 8 KB dump, as the development oracle.
    Real(Box<[u8; BIOS_SIZE]>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum MachineError {
    BadBiosSize(usize),
    CartTooLarge(usize),
}

/// One controller as the player holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    /// The left button, read in joystick mode.
    pub fire_left: bool,
    /// The right button, read in keypad mode.
    pub fire_right: bool,
    /// Keypad: 0-9, 10 for `*`, 11 for `#`.
    pub key: Option<u8>,
}

/// The four-bit code the keypad puts on the port for keys 0-9, `*`, `#`,
/// active low.
///
/// Checked against the real BIOS's own decoding table (16 bytes at `$10F5`,
/// indexed by the inverted nibble) on 2026-09-27: every digit decodes to
/// itself. The first version, written from the commonly published table,
/// had `*` and `#` the wrong way round: the BIOS decodes this `*` code to
/// `$0A` and this `#` code to `$0B`, the values OS7 documents for them.
const KEYPAD_CODES: [u8; 12] = [0x0a, 0x0d, 0x07, 0x0c, 0x02, 0x03, 0x0e, 0x05, 0x01, 0x0b, 0x09, 0x06];

impl Pad {
    /// The byte a read returns in joystick mode: directions in bits 0-3 and
    /// the left button in bit 6, all active low.
    fn joystick_byte(&self) -> u8 {
        let mut held = 0;
        if self.up {
            held |= 0x01;
        }
        if self.right {
            held |= 0x02;
        }
        if self.down {
            held |= 0x04;
        }
        if self.left {
            held |= 0x08;
        }
        if self.fire_left {
            held |= 0x40;
        }
        0x7f & !held
    }

    /// Keypad mode: the key's code in bits 0-3 (`$F` for none), the right
    /// button in bit 6.
    fn keypad_byte(&self) -> u8 {
        let code = match self.key {
            Some(k) if (k as usize) < KEYPAD_CODES.len() => KEYPAD_CODES[k as usize],
            _ => 0x0f,
        };
        let fire = if self.fire_right { 0 } else { 0x40 };
        0x30 | fire | code
    }
}

pub struct ColecoBus {
    bios: Box<[u8; BIOS_SIZE]>,
    cart: Vec<u8>,
    pub ram: [u8; WORK_RAM],
    pub vdp: Vdp,
    pub audio: Audio,
    pub pads: [Pad; 2],
    /// True after a write to `$80`-`$9F`, false after `$C0`-`$DF`.
    keypad_mode: bool,
    /// The BIOS census instrument, when a harness attaches one. Not state.
    pub probe: Option<Box<Probe>>,
    /// Cycles into the current scanline.
    line_cycles: i32,
    /// True between a line's `begin_line` and its `end_line`, so the machine
    /// can be stepped one instruction at a time and stop anywhere.
    in_line: bool,
    /// CPU cycles since power-on. A diagnostic, not machine state.
    pub cycles: u64,
}

impl ColecoBus {
    /// Let `cycles` of machine time pass: the PSG runs and the VDP draws and
    /// closes every scanline they cover. The CPU's steps come through here,
    /// and so can an HLE routine part-way through, which is how a routine
    /// that takes most of a frame on the real BIOS makes its later side
    /// effects (a status read, say) land after the lines it spans, as the
    /// real one's do. An NMI those lines raise is taken when the routine
    /// returns, not inside it: the one place the HLE's timing is coarser.
    pub fn spend(&mut self, cycles: i32) {
        self.line_cycles += cycles;
        self.cycles += cycles as u64;
        self.audio.run(cycles as u32);
        // No real instruction spans more than one line, so for the CPU this
        // is the single close it always was.
        while self.line_cycles >= CYCLES_PER_LINE {
            self.line_cycles -= CYCLES_PER_LINE;
            self.vdp.end_line();
            self.in_line = false;
            if self.line_cycles >= CYCLES_PER_LINE {
                self.vdp.begin_line();
                self.in_line = true;
            }
        }
    }

    /// A read with no side effects and no probe: for instrumentation.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1fff => self.bios[addr as usize],
            0x2000..=0x5fff => 0xff,
            0x6000..=0x7fff => self.ram[addr as usize & (WORK_RAM - 1)],
            _ => *self.cart.get(addr as usize - 0x8000).unwrap_or(&0xff),
        }
    }
}

impl Bus for ColecoBus {
    fn read(&mut self, addr: u16) -> u8 {
        if let Some(p) = &mut self.probe {
            p.read(addr);
        }
        self.peek(addr)
    }

    fn write(&mut self, addr: u16, val: u8) {
        if let Some(p) = &mut self.probe {
            p.write(addr);
        }
        if let 0x6000..=0x7fff = addr {
            self.ram[addr as usize & (WORK_RAM - 1)] = val;
        }
    }

    fn input(&mut self, port: u16) -> u8 {
        let p = port as u8;
        match p & 0xe0 {
            0xa0 => {
                if p & 1 == 0 {
                    self.vdp.read_data()
                } else {
                    self.vdp.read_control()
                }
            }
            0xe0 => {
                let pad = &self.pads[((p >> 1) & 1) as usize];
                if self.keypad_mode {
                    pad.keypad_byte()
                } else {
                    pad.joystick_byte()
                }
            }
            _ => 0xff,
        }
    }

    fn output(&mut self, port: u16, val: u8) {
        let p = port as u8;
        match p & 0xe0 {
            0x80 => self.keypad_mode = true,
            0xa0 => {
                if p & 1 == 0 {
                    self.vdp.write_data(val)
                } else {
                    self.vdp.write_control(val)
                }
            }
            0xc0 => self.keypad_mode = false,
            0xe0 => self.audio.psg.write(val),
            _ => {}
        }
    }
}

pub struct Coleco {
    pub cpu: Z80,
    pub bus: ColecoBus,
    /// The VDP interrupt line as last seen, for NMI edge detection.
    int_line: bool,
    /// NMIs taken since power-on. A diagnostic, not machine state.
    pub nmis: u64,
    /// True when running on the HLE, so BIOS-window PCs go through its traps.
    hle: bool,
    /// What the HLE could not serve. Diagnostics, not machine state.
    pub hle_log: crate::hle::HleLog,
}

impl Coleco {
    pub fn new(firmware: Firmware, cart: &[u8]) -> Result<Self, MachineError> {
        let hle = matches!(firmware, Firmware::Hle);
        let bios = match firmware {
            Firmware::Hle => crate::hle::image(),
            Firmware::Real(b) => b,
        };
        if cart.len() > MAX_CART {
            return Err(MachineError::CartTooLarge(cart.len()));
        }
        let bus = ColecoBus {
            bios,
            cart: cart.to_vec(),
            // Real SRAM powers up as noise. Zero is chosen for determinism;
            // the BIOS clears what it uses.
            ram: [0; WORK_RAM],
            vdp: Vdp::new(),
            audio: Audio::new(),
            pads: [Pad::default(); 2],
            keypad_mode: false,
            probe: None,
            line_cycles: 0,
            in_line: false,
            cycles: 0,
        };
        let mut m = Coleco {
            cpu: Z80::new(),
            bus,
            int_line: false,
            nmis: 0,
            hle,
            hle_log: Default::default(),
        };
        m.cpu.reset();
        Ok(m)
    }

    /// A BIOS image from a file's bytes.
    pub fn bios_from_bytes(b: &[u8]) -> Result<Firmware, MachineError> {
        let arr: Box<[u8; BIOS_SIZE]> =
            b.to_vec().into_boxed_slice().try_into().map_err(|_| MachineError::BadBiosSize(b.len()))?;
        Ok(Firmware::Real(arr))
    }

    /// Take an NMI if the VDP line has just gone active.
    fn poll_nmi(&mut self) -> i32 {
        let now = self.bus.vdp.irq();
        let edge = now && !self.int_line;
        self.int_line = now;
        if edge {
            self.nmis += 1;
            self.cpu.nmi(&mut self.bus)
        } else {
            0
        }
    }

    /// Execute one instruction (and an NMI first, if one is due), opening and
    /// closing scanlines as the cycle count crosses them. Everything else in
    /// the frame loop is built on this, so a harness that stops mid-frame sees
    /// exactly the machine a whole-frame run would.
    pub fn step(&mut self) {
        if !self.bus.in_line {
            self.bus.vdp.begin_line();
            self.bus.in_line = true;
        }
        {
            let interrupted = self.cpu.pc;
            let mut c = self.poll_nmi();
            if self.bus.probe.is_some() {
                let pc = self.cpu.pc;
                if c > 0 {
                    self.bus.probe.as_mut().unwrap().nmi_from(interrupted);
                }
                let op = [self.bus.peek(pc), self.bus.peek(pc.wrapping_add(1))];
                let p = self.bus.probe.as_mut().unwrap();
                p.cycles(pc, c);
                p.instruction(pc, op, c > 0);
                if p.cart_seen && p.handover.is_none() {
                    p.handover = Some(self.bus.ram.to_vec());
                }
            }
            let pc = self.cpu.pc;
            let trapped = if self.hle && pc < 0x2000 {
                crate::hle::trap(&mut self.cpu, &mut self.bus, &mut self.hle_log)
            } else {
                None
            };
            let s = match trapped {
                Some(c) => c,
                None => self.cpu.step(&mut self.bus),
            };
            if self.hle {
                self.hle_log.prev_pc = pc;
            }
            if let Some(p) = &mut self.bus.probe {
                p.cycles(pc, s);
            }
            c += s;
            self.bus.spend(c);
        }
    }

    /// The cartridge the machine was built with.
    pub fn cartridge(&self) -> &[u8] {
        &self.bus.cart
    }

    /// CPU cycles since power-on, a routine's own spending included.
    pub fn cycles(&self) -> u64 {
        self.bus.cycles
    }

    /// True while a scanline is open: false exactly between lines, which is
    /// where a frame boundary falls when `vdp.line` is back to 0.
    pub fn in_line(&self) -> bool {
        self.bus.in_line
    }

    pub fn run_line(&mut self) {
        loop {
            self.step();
            if !self.bus.in_line {
                break;
            }
        }
    }

    pub fn run_frame(&mut self) {
        for _ in 0..vdp::LINES_PER_FRAME {
            self.run_line();
        }
    }

    pub fn framebuffer(&self) -> &[u32] {
        &self.bus.vdp.framebuffer[..]
    }

    pub fn take_audio(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.bus.audio.out)
    }

    /// Machine state. Firmware and cartridge are content, not state, and are
    /// not included: a state loads into a machine built from the same ones.
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = WriteCursor::new();
        w.bytes(STATE_MAGIC);
        w.u16(crate::SAVE_STATE_VERSION as u16);
        self.cpu.save(&mut w);
        w.bytes(&self.bus.ram);
        self.bus.vdp.save(&mut w);
        self.bus.audio.save(&mut w);
        w.bool(self.bus.keypad_mode);
        w.bool(self.int_line);
        w.i32(self.bus.line_cycles);
        w.bool(self.bus.in_line);
        w.into_bytes()
    }

    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), LoadError> {
        let mut r = ReadCursor::new(bytes);
        let mut magic = [0u8; 8];
        r.bytes(&mut magic)?;
        if &magic != STATE_MAGIC {
            return Err(LoadError::BadMagic);
        }
        let v = r.u16()?;
        if v != crate::SAVE_STATE_VERSION as u16 {
            return Err(LoadError::UnsupportedVersion(v));
        }
        self.cpu.load(&mut r)?;
        r.bytes(&mut self.bus.ram)?;
        self.bus.vdp.load(&mut r)?;
        self.bus.audio.load(&mut r)?;
        self.bus.keypad_mode = r.bool()?;
        self.int_line = r.bool()?;
        self.bus.line_cycles = r.i32()?;
        self.bus.in_line = r.bool()?;
        r.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BIOS that is all `NOP` except what a test puts in it.
    fn machine_with(program: &[u8], at: usize) -> Coleco {
        let mut bios = vec![0u8; BIOS_SIZE];
        bios[at..at + program.len()].copy_from_slice(program);
        Coleco::new(Coleco::bios_from_bytes(&bios).unwrap(), &[]).unwrap()
    }

    #[test]
    fn hle_boots_a_skip_header_cart_straight_to_its_start() {
        // $55AA: no title screen. Start address $8123 at $800A.
        let mut cart = vec![0u8; 0x100];
        cart[0..2].copy_from_slice(&[0x55, 0xaa]);
        cart[0x0a..0x0c].copy_from_slice(&[0x23, 0x81]);
        let mut m = Coleco::new(Firmware::Hle, &cart).unwrap();
        m.step();
        assert_eq!((m.cpu.pc, m.cpu.sp, m.cpu.hl()), (0x8123, 0x73b9, 0x8123));
    }

    #[test]
    fn hle_hands_a_title_header_cart_the_title_screens_machine() {
        let mut cart = vec![0u8; 0x100];
        cart[0..2].copy_from_slice(&[0xaa, 0x55]);
        cart[0x0a..0x0c].copy_from_slice(&[0x50, 0x80]);
        let mut m = Coleco::new(Firmware::Hle, &cart).unwrap();
        m.step();
        assert_eq!(m.cpu.pc, 0x8050);
        assert_eq!(m.bus.vdp.regs, [0x00, 0x80, 0x06, 0x80, 0x00, 0x36, 0x07, 0x00]);
        assert_eq!(m.bus.ram[0x3c4], 0x80, "the register 1 shadow");
        assert_ne!(m.bus.vdp.vram[b'A' as usize * 8..b'A' as usize * 8 + 8], [0u8; 8], "font loaded");
    }

    #[test]
    fn hle_parks_a_cart_with_no_header() {
        let mut m = Coleco::new(Firmware::Hle, &[0u8; 0x100]).unwrap();
        for _ in 0..3 {
            m.run_frame();
        }
        // This Z80 holds the PC on the HALT while halted.
        assert_eq!(m.cpu.pc, 0x0003, "parked on the HALT at $0003");
        assert!(m.cpu.halted);
    }

    /// An unwritten routine returns to its caller and is logged.
    #[test]
    fn an_unwritten_routine_returns_and_is_logged() {
        // Cart: at $8100, CALL $1F6A (REFLECT_VERTICAL, not written yet), then JR $ (spin).
        let mut cart = vec![0u8; 0x200];
        cart[0..2].copy_from_slice(&[0x55, 0xaa]);
        cart[0x0a..0x0c].copy_from_slice(&[0x00, 0x81]);
        cart[0x100..0x105].copy_from_slice(&[0xcd, 0x6a, 0x1f, 0x18, 0xfe]);
        let mut m = Coleco::new(Firmware::Hle, &cart).unwrap();
        m.run_frame();
        assert_eq!(m.cpu.pc, 0x8103, "back after the CALL, spinning");
        assert_eq!(m.hle_log.unimplemented.get(&0x1d5a), Some(&1));
    }

    #[test]
    fn one_kilobyte_of_ram_is_mirrored_eight_times() {
        let mut m = machine_with(&[], 0);
        m.bus.write(0x6005, 0x42);
        for mirror in 0..8u16 {
            assert_eq!(m.bus.read(0x6005 + mirror * 0x400), 0x42);
        }
        assert_eq!(m.bus.read(0x7fff), m.bus.ram[0x3ff]);
    }

    #[test]
    fn the_bios_is_not_writable_and_open_bus_reads_ff() {
        let mut m = machine_with(&[0x12], 0x100);
        m.bus.write(0x0100, 0x99);
        assert_eq!(m.bus.read(0x0100), 0x12);
        assert_eq!(m.bus.read(0x2000), 0xff);
        assert_eq!(m.bus.read(0x8000), 0xff, "no cartridge");
    }

    /// Ports decode on bits 7-5: `$BE` and `$A0` are the same data port,
    /// `$BF` and `$A1` the same control port.
    #[test]
    fn vdp_ports_decode_on_the_top_three_bits() {
        let mut m = machine_with(&[], 0);
        m.bus.output(0xa1, 0x07);
        m.bus.output(0xbf, 0x87); // register 7 = 7, via a mirror
        assert_eq!(m.bus.vdp.regs[7], 0x07);
    }

    #[test]
    fn controller_mode_follows_the_last_strobe() {
        let mut m = machine_with(&[], 0);
        m.bus.pads[0] = Pad { up: true, key: Some(1), ..Pad::default() };
        m.bus.output(0xc0, 0);
        assert_eq!(m.bus.input(0xfc) & 0x0f, 0x0e, "joystick: up is bit 0, active low");
        m.bus.output(0x80, 0);
        assert_eq!(m.bus.input(0xfc) & 0x0f, KEYPAD_CODES[1]);
        assert_eq!(m.bus.input(0xff) & 0x0f, 0x0f, "port 2 has no key down");
    }

    /// The VDP interrupt is an NMI and is edge-triggered: F staying set does
    /// not interrupt again until the status read drops the line and it rises
    /// once more.
    #[test]
    fn the_vdp_interrupt_is_one_nmi_per_rising_edge() {
        // At $0066, the NMI vector: RETN. The main program spins on NOPs.
        let mut m = machine_with(&[0xed, 0x45], 0x66);
        m.bus.vdp.regs[1] = 0x20;
        for _ in 0..3 {
            m.run_frame();
        }
        assert_eq!(m.nmis, 1, "nobody read status, so F never dropped");

        // Now a handler that reads status (IN A,($BF)) before returning.
        let mut m = machine_with(&[0xdb, 0xbf, 0xed, 0x45], 0x66);
        m.bus.vdp.regs[1] = 0x20;
        for _ in 0..3 {
            m.run_frame();
        }
        assert_eq!(m.nmis, 3, "one per frame");
    }

    /// A routine charging most of a frame advances the VDP by that many
    /// lines, not one.
    #[test]
    fn a_long_hle_routine_advances_the_scanlines_it_covers() {
        // FILL_VRAM of 4096 bytes from the cartridge: CALL $1F82, then spin.
        let mut cart = vec![0u8; 0x200];
        cart[0..2].copy_from_slice(&[0x55, 0xaa]);
        cart[0x0a..0x0c].copy_from_slice(&[0x00, 0x81]);
        cart[0x100..0x10c].copy_from_slice(&[
            0x21, 0x00, 0x00, // LD HL,0
            0x11, 0x00, 0x10, // LD DE,$1000
            0xcd, 0x82, 0x1f, // CALL FILL_VRAM
            0x18, 0xfe, 0x00, // JR $
        ]);
        let mut m = Coleco::new(Firmware::Hle, &cart).unwrap();
        while m.cpu.pc != 0x8109 {
            m.step();
        }
        // 4096 bytes at about 41 cycles each is over 700 lines.
        assert!(m.cycles() > 4096 * 41);
        let lines = m.cycles() / CYCLES_PER_LINE as u64;
        let frames_line = (lines % vdp::LINES_PER_FRAME as u64) as u16;
        assert_eq!(m.bus.vdp.line, frames_line, "the VDP kept pace with the cycles");
    }

    #[test]
    fn a_state_round_trips_and_resumes_identically() {
        let mut a = machine_with(&[0xdb, 0xbf, 0xed, 0x45], 0x66);
        a.bus.vdp.regs[1] = 0x20;
        a.run_frame();
        let s = a.save_state();
        let mut b = machine_with(&[0xdb, 0xbf, 0xed, 0x45], 0x66);
        b.load_state(&s).unwrap();
        assert_eq!(b.save_state(), s);
        a.run_frame();
        b.run_frame();
        assert_eq!(a.save_state(), b.save_state());
    }
}
