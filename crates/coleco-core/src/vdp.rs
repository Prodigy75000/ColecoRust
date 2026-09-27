// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The TI TMS9918A, the ColecoVision's video chip.
//!
//! **Derived from MegaRust's `crates/md-core/src/sms/vdp.rs`** (`origin/main`
//! 3e97ca7), which runs the TMS9918 modes as the legacy half of Sega's
//! Master System VDP. The rendering of Graphics I, Multicolor, Text and the
//! sprites is lifted from there unchanged. What changed, because this is TI's
//! chip and not Sega's copy of it:
//!
//! - **No mode 4, no CRAM, no line interrupt, no Game Gear.** On a real
//!   TMS9918A register 0 bit 2 does nothing, so a game that sets it must not
//!   fall into a Master System mode.
//! - **Eight registers, three bits of register number.** A second control byte
//!   with bit 7 set is a register write whatever bit 6 says, and the register
//!   is its low three bits (TMS s2.1.2). Sega's chip reads four bits and has
//!   eleven registers, and uses `$C0` for CRAM.
//! - **Graphics II masks.** See [`Vdp::g2_addresses`].
//!
//! Sections cited as "TMS s2.1.2" are the TI data manual's, through MegaRust's
//! clean-room notes `docs/video/tms9918-notes.md`, which also list what the
//! manual leaves undocumented ("Ambiguities"). Where this file picks a side on
//! one of those, it says so.

use crate::save::{LoadError, ReadCursor, SaveState, WriteCursor};

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;
pub const VRAM_SIZE: usize = 0x4000;
/// NTSC. A PAL ColecoVision exists; it is not modelled yet.
pub const LINES_PER_FRAME: u16 = 262;

/// Status bits (TMS s2.3). The low five carry the fifth sprite's number.
const STATUS_INT: u8 = 0x80;
const STATUS_5S: u8 = 0x40;
const STATUS_COL: u8 = 0x20;

/// Sprite attribute table terminator (TMS s5).
const SPRITE_END: u8 = 0xd0;

/// The TMS9918's fixed sixteen colours (TMS s6). The manual gives luminance and
/// colour difference, not RGB: this is the TMS9928A's Y, R-Y and B-Y, with 0.47
/// taken as zero colour difference and the full 0-1 range as the Pb/Pr swing,
/// through the standard Y'PbPr to RGB matrix. Colour 0 is transparent and is
/// never looked up; black stands in for it.
const TMS_PALETTE: [u32; 16] = [
    0x000000, 0x000000, 0x00e80d, 0x40f350, 0x4d44ff, 0x7966ff, 0xf9452b, 0x12fcff, 0xff452d,
    0xff6950, 0xdecb05, 0xf0d444, 0x00cb0b, 0xe446e2, 0xcccccc, 0xffffff,
];

/// Which mode the registers select (TMS s2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TmsMode {
    Graphics1,
    Graphics2,
    Multicolor,
    Text,
}

pub struct Vdp {
    pub vram: Box<[u8; VRAM_SIZE]>,
    pub regs: [u8; 8],
    /// The 14-bit address register.
    addr: u16,
    /// Set between the two bytes of a command word.
    second_byte: bool,
    /// The first byte, kept for a register write.
    first_byte: u8,
    read_buffer: u8,
    /// F, 5S and C. The fifth-sprite number is kept apart in `fifth`.
    status: u8,
    fifth: u8,
    /// Scanline being processed, 0 at the first active line.
    pub line: u16,
    /// XRGB8888, `WIDTH` pixels per row.
    pub framebuffer: Box<[u32; WIDTH * HEIGHT]>,
}

impl Default for Vdp {
    fn default() -> Self {
        Self::new()
    }
}

impl Vdp {
    pub fn new() -> Self {
        Vdp {
            vram: Box::new([0; VRAM_SIZE]),
            regs: [0; 8],
            addr: 0,
            second_byte: false,
            first_byte: 0,
            read_buffer: 0,
            status: 0,
            fifth: 0,
            line: 0,
            framebuffer: Box::new([0xff00_0000; WIDTH * HEIGHT]),
        }
    }

    /// True while the chip holds its INT output low. On a ColecoVision that
    /// line is the Z80's NMI, which is edge-triggered, so the machine watches
    /// for this going from false to true rather than for its level.
    pub fn irq(&self) -> bool {
        self.status & STATUS_INT != 0 && self.regs[1] & 0x20 != 0
    }

    // ---- ports ----

    pub fn write_control(&mut self, v: u8) {
        if !self.second_byte {
            // Kept from the Sega chip: the low address byte lands at once.
            // The TI manual does not say either way (Ambiguities 8).
            self.first_byte = v;
            self.addr = (self.addr & 0x3f00) | v as u16;
            self.second_byte = true;
            return;
        }
        self.second_byte = false;
        if v & 0x80 != 0 {
            // Register write: bits 6..3 should be 0 and are ignored, bits 2..0
            // pick the register (TMS s2.1.2).
            self.regs[(v & 0x07) as usize] = self.first_byte;
            return;
        }
        self.addr = (self.addr & 0x00ff) | (((v & 0x3f) as u16) << 8);
        if v & 0x40 == 0 {
            // Read setup prefetches, so the first data read is already there.
            self.read_buffer = self.vram[self.addr as usize];
            self.bump();
        }
    }

    /// Status flags, cleared by the read, along with the command-word latch.
    /// The low five bits carry the fifth sprite's number (TMS s2.3).
    pub fn read_control(&mut self) -> u8 {
        let s = self.status | (self.fifth & 0x1f);
        self.status = 0;
        self.second_byte = false;
        s
    }

    pub fn write_data(&mut self, v: u8) {
        self.second_byte = false;
        self.vram[self.addr as usize] = v;
        // Writing loads the read buffer too. Undocumented (Ambiguities 7);
        // kept from the Sega chip, where it is measured.
        self.read_buffer = v;
        self.bump();
    }

    pub fn read_data(&mut self) -> u8 {
        self.second_byte = false;
        let r = self.read_buffer;
        self.read_buffer = self.vram[self.addr as usize];
        self.bump();
        r
    }

    fn bump(&mut self) {
        self.addr = (self.addr + 1) & 0x3fff;
    }

    // ---- the line ----

    /// Start line `self.line`: draw it if it is active, and raise F on the
    /// first line after the active area. A TMS9918 sets F at the end of its
    /// last active line (TMS s3, s7), so the next line starts with it.
    pub fn begin_line(&mut self) {
        let line = self.line as usize;
        if line < HEIGHT {
            self.render_line(line);
        }
        if line == HEIGHT {
            self.status |= STATUS_INT;
        }
    }

    /// Finish the current line and move to the next.
    pub fn end_line(&mut self) {
        self.line += 1;
        if self.line >= LINES_PER_FRAME {
            self.line = 0;
        }
    }

    fn tms_mode(&self) -> TmsMode {
        let m1 = self.regs[1] & 0x10 != 0;
        let m2 = self.regs[1] & 0x08 != 0;
        let m3 = self.regs[0] & 0x02 != 0;
        // Only the four single-bit combinations are documented (TMS s2); the
        // others take the first match here.
        if m1 {
            TmsMode::Text
        } else if m2 {
            TmsMode::Multicolor
        } else if m3 {
            TmsMode::Graphics2
        } else {
            TmsMode::Graphics1
        }
    }

    fn tms_colour(c: u8) -> u32 {
        0xff00_0000 | TMS_PALETTE[(c & 15) as usize]
    }

    fn render_line(&mut self, line: usize) {
        let row = line * WIDTH;
        let backdrop = Self::tms_colour(self.regs[7] & 0x0f);
        if self.regs[1] & 0x40 == 0 {
            // BLANK: the whole line is the border colour (TMS s8).
            self.framebuffer[row..row + WIDTH].fill(backdrop);
            return;
        }
        let mut bg = [0u8; WIDTH];
        self.tms_background(line, &mut bg);
        let mut spr = [0u8; WIDTH];
        if self.tms_mode() != TmsMode::Text {
            self.tms_sprites(line, &mut spr);
        }
        for x in 0..WIDTH {
            // Front to back: sprites, the pattern plane, the backdrop. Colour 0
            // is transparent on every plane (TMS s4).
            let c = if spr[x] != 0 { spr[x] } else { bg[x] };
            self.framebuffer[row + x] = if c != 0 {
                Self::tms_colour(c)
            } else {
                backdrop
            };
        }
    }

    /// Graphics II pattern and colour addresses for name `n`, row `y` of the
    /// cell, in screen third `third`.
    ///
    /// The manual says the low bits of registers 3 and 4 "must be set to all
    /// 1s" and documents nothing else (Ambiguities 1). Real TMS9918As are
    /// widely reported to use them as AND masks on the address: register 4
    /// bits 1-0 against address bits 12-11 (the third), register 3 bits 6-0
    /// against address bits 12-6. Some ColecoVision titles lean on that to
    /// share one pattern table across the screen. That behaviour is taken
    /// here, and is NOT from our sources; the corpus is the check. With the
    /// documented all-1s values this is exactly the unmasked layout.
    fn g2_addresses(&self, third: usize, n: usize, y: usize) -> (usize, usize) {
        let offset = (third << 11) | (n << 3) | y;
        let pg = ((self.regs[4] & 0x04) as usize) << 11;
        let pmask = (((self.regs[4] & 0x03) as usize) << 11) | 0x7ff;
        let ct = ((self.regs[3] & 0x80) as usize) << 6;
        let cmask = (((self.regs[3] & 0x7f) as usize) << 6) | 0x3f;
        (pg | (offset & pmask), ct | (offset & cmask))
    }

    /// The pattern plane as colour codes, 0 where it is transparent (TMS s4).
    fn tms_background(&self, line: usize, out: &mut [u8; WIDTH]) {
        let v = |a: usize| self.vram[a & 0x3fff];
        let names = ((self.regs[2] & 0x0f) as usize) << 10;
        let patterns = ((self.regs[4] & 0x07) as usize) << 11;
        let colours = (self.regs[3] as usize) << 6;
        let r = line >> 3;
        let y = line & 7;
        match self.tms_mode() {
            TmsMode::Graphics1 => {
                for x in 0..WIDTH {
                    let n = v(names + r * 32 + (x >> 3)) as usize;
                    let bits = v(patterns + n * 8 + y);
                    let col = v(colours + (n >> 3));
                    out[x] = if bits & (0x80 >> (x & 7)) != 0 {
                        col >> 4
                    } else {
                        col & 15
                    };
                }
            }
            TmsMode::Graphics2 => {
                let third = line >> 6;
                for x in 0..WIDTH {
                    let n = v(names + r * 32 + (x >> 3)) as usize;
                    let (pa, ca) = self.g2_addresses(third, n, y);
                    let bits = v(pa);
                    let col = v(ca);
                    out[x] = if bits & (0x80 >> (x & 7)) != 0 {
                        col >> 4
                    } else {
                        col & 15
                    };
                }
            }
            TmsMode::Multicolor => {
                for x in 0..WIDTH {
                    let n = v(names + r * 32 + (x >> 3)) as usize;
                    let b = v(patterns + n * 8 + 2 * (r & 3) + (y >> 2));
                    out[x] = if x & 7 < 4 { b >> 4 } else { b & 15 };
                }
            }
            TmsMode::Text => {
                // 40 six-pixel columns, 240 wide, with a left border six pixels
                // wider than the other modes' (TMS s4: 19 against 13).
                let fg = self.regs[7] >> 4;
                let bgc = self.regs[7] & 15;
                for x in 6..6 + 240 {
                    let c = (x - 6) / 6;
                    let px = (x - 6) % 6;
                    let n = v(names + r * 40 + c) as usize;
                    let bits = v(patterns + n * 8 + y);
                    out[x] = if bits & (0x80 >> px) != 0 { fg } else { bgc };
                }
            }
        }
    }

    /// The TMS9918's sprites on one line: 32 of them, four to a line, each one
    /// colour (TMS s5). Sets 5S with the fifth sprite's number, and C.
    fn tms_sprites(&mut self, line: usize, out: &mut [u8; WIDTH]) {
        let sat = ((self.regs[5] & 0x7f) as usize) << 7;
        let spg = ((self.regs[6] & 0x07) as usize) << 11;
        let big = self.regs[1] & 0x02 != 0;
        let mag: i32 = if self.regs[1] & 0x01 != 0 { 2 } else { 1 };
        let size: i32 = if big { 16 } else { 8 };
        let h = size * mag;

        let mut on_line = [0usize; 4];
        let mut n = 0;
        for i in 0..32 {
            let yb = self.vram[sat + i * 4];
            if yb == SPRITE_END {
                break;
            }
            // Drawn from Y+1, and values past the terminator are the -31..-1
            // that let a sprite slide in from the top (TMS s5).
            let top = if yb > 0xd0 {
                yb as i32 - 256
            } else {
                yb as i32
            } + 1;
            let l = line as i32;
            if l < top || l >= top + h {
                continue;
            }
            if n == 4 {
                if self.status & (STATUS_INT | STATUS_5S) == 0 {
                    self.status |= STATUS_5S;
                    self.fifth = i as u8;
                }
                break;
            }
            on_line[n] = i;
            n += 1;
        }

        // Every 1 bit of the sprites that made the line, whatever its colour,
        // for the coincidence flag (TMS s3).
        let mut solid = [false; WIDTH];
        for &i in &on_line[..n] {
            let e = sat + i * 4;
            let yb = self.vram[e];
            let top = if yb > 0xd0 {
                yb as i32 - 256
            } else {
                yb as i32
            } + 1;
            let mut x = self.vram[e + 1] as i32;
            let mut name = self.vram[e + 2] as usize;
            let attr = self.vram[e + 3];
            if attr & 0x80 != 0 {
                x -= 32;
            }
            let colour = attr & 0x0f;
            let row = ((line as i32 - top) / mag) as usize;
            if big {
                name &= 0xfc;
            }
            for px in 0..h {
                let sx = x + px;
                if !(0..WIDTH as i32).contains(&sx) {
                    continue;
                }
                let col = (px / mag) as usize;
                // A 16x16 pattern is four 8x8 quadrants stored top-left,
                // bottom-left, top-right, bottom-right (TMS s5).
                let addr = spg + name * 8 + (col >> 3) * 16 + row;
                if self.vram[addr & 0x3fff] & (0x80 >> (col & 7)) == 0 {
                    continue;
                }
                let sx = sx as usize;
                if solid[sx] {
                    self.status |= STATUS_COL;
                }
                solid[sx] = true;
                // The lower-numbered sprite is in front, and a transparent
                // pixel lets the next plane show through (TMS s6).
                if colour != 0 && out[sx] == 0 {
                    out[sx] = colour;
                }
            }
        }
    }
}

impl SaveState for Vdp {
    fn save(&self, w: &mut WriteCursor) {
        w.bytes(&self.vram[..]);
        w.bytes(&self.regs);
        w.u16(self.addr);
        w.bool(self.second_byte);
        w.u8(self.first_byte);
        w.u8(self.read_buffer);
        w.u8(self.status);
        w.u8(self.fifth);
        w.u16(self.line);
    }

    fn load(&mut self, r: &mut ReadCursor) -> Result<(), LoadError> {
        r.bytes(&mut self.vram[..])?;
        r.bytes(&mut self.regs)?;
        self.addr = r.u16()?;
        if self.addr > 0x3fff {
            return Err(LoadError::BadValue("vdp address"));
        }
        self.second_byte = r.bool()?;
        self.first_byte = r.u8()?;
        self.read_buffer = r.u8()?;
        self.status = r.u8()?;
        self.fifth = r.u8()?;
        self.line = r.u16()?;
        if self.line >= LINES_PER_FRAME {
            return Err(LoadError::BadValue("vdp line"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_reg(v: &mut Vdp, r: u8, val: u8) {
        v.write_control(val);
        v.write_control(0x80 | r);
    }

    fn set_addr(v: &mut Vdp, addr: u16, write: bool) {
        v.write_control(addr as u8);
        v.write_control(((addr >> 8) as u8 & 0x3f) | if write { 0x40 } else { 0 });
    }

    /// TI's chip reads three bits of register number: "register 8" is
    /// register 0, and a `$C0` second byte is a register write, not a CRAM
    /// address as on Sega's chip.
    #[test]
    fn the_register_number_is_three_bits_and_bit_6_is_ignored() {
        let mut v = Vdp::new();
        set_reg(&mut v, 7, 0x0c);
        assert_eq!(v.regs[7], 0x0c);
        set_reg(&mut v, 8, 0x02);
        assert_eq!(v.regs[0], 0x02, "register 8 wraps to 0");
        v.write_control(0x55);
        v.write_control(0xc3);
        assert_eq!(v.regs[3], 0x55, "$C3 writes register 3");
    }

    /// Register 0 bit 2 is Sega's mode 4. On a TMS9918A it does nothing, so a
    /// game that sets it still gets its TMS picture.
    #[test]
    fn the_sega_mode_4_bit_changes_nothing() {
        let mut a = Vdp::new();
        set_reg(&mut a, 1, 0x40);
        set_reg(&mut a, 7, 0x04);
        a.vram[0] = 0xff;
        let mut b = Vdp::new();
        set_reg(&mut b, 1, 0x40);
        set_reg(&mut b, 7, 0x04);
        b.vram[0] = 0xff;
        set_reg(&mut b, 0, 0x04);
        a.render_line(0);
        b.render_line(0);
        assert_eq!(a.framebuffer[..WIDTH], b.framebuffer[..WIDTH]);
    }

    /// Reads are one behind: the buffer is filled when the address is set for
    /// reading, and each read hands it over and fetches the next byte.
    #[test]
    fn vram_reads_come_through_the_buffer() {
        let mut v = Vdp::new();
        v.vram[0x1234] = 0xaa;
        v.vram[0x1235] = 0xbb;
        set_addr(&mut v, 0x1234, false);
        assert_eq!(v.read_data(), 0xaa);
        assert_eq!(v.read_data(), 0xbb);
    }

    #[test]
    fn the_address_wraps_at_sixteen_k() {
        let mut v = Vdp::new();
        set_addr(&mut v, 0x3fff, true);
        v.write_data(1);
        v.write_data(2);
        assert_eq!(v.vram[0x3fff], 1);
        assert_eq!(v.vram[0x0000], 2);
    }

    /// F goes up at the start of line 192 and reading status clears it, and
    /// with it the interrupt.
    #[test]
    fn f_rises_on_line_192_and_a_status_read_clears_it() {
        let mut v = Vdp::new();
        set_reg(&mut v, 1, 0x20);
        for _ in 0..192 {
            v.begin_line();
            v.end_line();
        }
        assert!(!v.irq(), "nothing during the active area");
        v.begin_line();
        assert!(v.irq(), "line 192 raised F");
        assert_eq!(v.read_control() & 0x80, 0x80);
        assert!(!v.irq());
        assert_eq!(v.read_control() & 0x80, 0, "and it stays clear");
    }

    /// Graphics I: one colour byte per eight patterns, 1 bits in its high
    /// nibble and 0 bits in its low one (TMS s4).
    #[test]
    fn graphics_one_colours_come_from_the_group_byte() {
        let mut v = Vdp::new();
        set_reg(&mut v, 1, 0x40);
        set_reg(&mut v, 2, 0x06);
        set_reg(&mut v, 3, 0x80);
        set_reg(&mut v, 4, 0x00);
        set_reg(&mut v, 7, 0x01);
        v.vram[0x1800] = 9;
        v.vram[9 * 8] = 0xf0;
        v.vram[0x2000 + 1] = 0x4f;
        v.render_line(0);
        assert_eq!(v.framebuffer[0], Vdp::tms_colour(4));
        assert_eq!(v.framebuffer[4], Vdp::tms_colour(15));
    }

    fn graphics_two(r3: u8, r4: u8) -> Vdp {
        let mut v = Vdp::new();
        set_reg(&mut v, 0, 0x02);
        set_reg(&mut v, 1, 0x40);
        set_reg(&mut v, 2, 0x0e); // names at $3800
        set_reg(&mut v, 3, r3);
        set_reg(&mut v, 4, r4);
        v.vram[0x3800] = 1;
        v.vram[0x3800 + 8 * 32] = 1; // row 8, the second third
        v.vram[8] = 0xff; // third 0, pattern 1, row 0: all set
        v.vram[0x2000 + 8] = 0x20;
        v.vram[0x0800 + 8] = 0x00; // third 1, pattern 1, row 0: all clear
        v.vram[0x2800 + 8] = 0x0b;
        v
    }

    /// With the documented all-1s masks, each third has its own patterns and
    /// colours.
    #[test]
    fn graphics_two_splits_the_screen_in_thirds() {
        let mut v = graphics_two(0xff, 0x03);
        v.render_line(0);
        assert_eq!(v.framebuffer[0], Vdp::tms_colour(2));
        v.render_line(64);
        assert_eq!(v.framebuffer[64 * WIDTH], Vdp::tms_colour(0x0b));
    }

    /// Masks of zero fold every third onto the first: line 64 reads third 0's
    /// pattern and colour, which the Sega chip would never do.
    #[test]
    fn graphics_two_masks_fold_the_thirds() {
        let mut v = graphics_two(0x80, 0x00);
        v.render_line(64);
        assert_eq!(v.framebuffer[64 * WIDTH], Vdp::tms_colour(2));
    }

    /// Five sprites on a line: four are drawn, 5S is set and names the fifth.
    #[test]
    fn a_fifth_sprite_is_dropped_and_reported() {
        let mut v = Vdp::new();
        set_reg(&mut v, 1, 0x40);
        set_reg(&mut v, 5, 0x7e);
        set_reg(&mut v, 6, 0x00);
        for r in 0..8 {
            v.vram[8 + r] = 0xff;
        }
        for i in 0..5 {
            let e = 0x3f00 + i * 4;
            v.vram[e] = 9;
            v.vram[e + 1] = (i * 20) as u8;
            v.vram[e + 2] = 1;
            v.vram[e + 3] = 0x06;
        }
        v.vram[0x3f00 + 20] = 0xd0;
        v.render_line(10);
        assert_eq!(
            v.read_control() & 0x5f,
            0x40 | 4,
            "5S, and sprite 4 was fifth"
        );
        assert_eq!(
            v.framebuffer[10 * WIDTH + 60],
            Vdp::tms_colour(6),
            "sprite 3 drawn"
        );
        assert_eq!(
            v.framebuffer[10 * WIDTH + 80],
            Vdp::tms_colour(0),
            "sprite 4 is not"
        );
    }

    #[test]
    fn state_round_trips_at_a_fixed_size() {
        let mut v = Vdp::new();
        set_reg(&mut v, 2, 0x0f);
        v.vram[77] = 7;
        v.line = 100;
        let mut w = WriteCursor::new();
        v.save(&mut w);
        let bytes = w.into_bytes();
        assert_eq!(bytes.len(), 16_384 + 8 + 2 + 1 + 1 + 1 + 1 + 1 + 2);
        let mut back = Vdp::new();
        back.load(&mut ReadCursor::new(&bytes)).unwrap();
        let mut w = WriteCursor::new();
        back.save(&mut w);
        assert_eq!(w.into_bytes(), bytes);
    }
}
