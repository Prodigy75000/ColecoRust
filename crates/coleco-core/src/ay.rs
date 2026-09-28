// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The General Instrument AY-3-8910 on the Super Game Module.
//!
//! **Sources.** Written from the AY-3-8910 datasheet's description of the
//! chip as every emulator and the homebrew community repeat it, not from any
//! one implementation: sixteen registers, three square-wave channels with
//! 12-bit periods, one noise source with a 5-bit period, a mixer that can
//! gate each channel's tone and noise, 4-bit levels, and one envelope
//! generator with eight useful shapes. None of my other cores has this chip to lift.
//!
//! **Clock.** The SGM runs it at half the Z80's clock. The chip divides by 8
//! before its tone counters, so a tone counter ticks once per 16 CPU clocks,
//! the same step the SN76489 takes, and both chips advance together in
//! [`crate::psg::Audio`]:
//!
//! - a tone flips every `period` ticks, a full cycle of `16 * period` chip
//!   clocks, the datasheet's `f = clock / (16 * TP)`;
//! - the noise register shifts every `2 * period` ticks (`clock / (16 * NP)`
//!   for its rate is the rate the shift is TESTED at, and it takes two tests
//!   to shift, as the tone flips twice per cycle);
//! - the envelope takes one of its sixteen steps every `2 * period` ticks,
//!   a whole ramp in `256 * EP` chip clocks, the datasheet's figure.
//!
//! A period of zero counts as one, as on the chip.
//!
//! **Levels.** The datasheet's level curve is logarithmic, about 3 dB a
//! step. The chip's output is unipolar: a channel is its level or nothing,
//! and a level of zero is exactly silence.

use crate::save::{LoadError, ReadCursor, SaveState, WriteCursor};

/// Full scale is the SN76489's, 8191, then 3 dB (a factor of 1/sqrt 2) a
/// step down, and step 0 is silent.
const LEVEL: [i16; 16] = [0, 64, 90, 128, 181, 256, 362, 512, 724, 1024, 1448, 2048, 2896, 4096, 5792, 8191];

/// The bits of each register the chip keeps; the rest read back as 0.
const MASK: [u8; 16] = [0xff, 0x0f, 0xff, 0x0f, 0xff, 0x0f, 0x1f, 0xff, 0x1f, 0x1f, 0x1f, 0xff, 0xff, 0x0f, 0xff, 0xff];

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ay {
    regs: [u8; 16],
    /// The register the next data write goes to, from the last select.
    selected: u8,
    tone_count: [u16; 3],
    tone_high: [bool; 3],
    noise_count: u16,
    /// Seventeen bits, tapped at 0 and 3; bit 0 is the output.
    lfsr: u32,
    env_count: u32,
    /// Signed so the step below 0 can be seen: 15 down to 0.
    env_step: i8,
    /// XORed into the step: 0 for a fall, 15 for a rise.
    env_attack: u8,
    env_hold: bool,
    env_alternate: bool,
    env_holding: bool,
}

impl Default for Ay {
    fn default() -> Self {
        Self::new()
    }
}

impl Ay {
    pub fn new() -> Self {
        Ay {
            regs: [0; 16],
            selected: 0,
            tone_count: [0; 3],
            tone_high: [false; 3],
            noise_count: 0,
            lfsr: 1,
            env_count: 0,
            env_step: 0,
            env_attack: 0,
            env_hold: true,
            env_alternate: false,
            env_holding: true,
        }
    }

    /// Port `$50`: choose a register.
    pub fn select(&mut self, val: u8) {
        self.selected = val & 0x0f;
    }

    /// Port `$51`: write the chosen register.
    pub fn write(&mut self, val: u8) {
        let r = self.selected as usize;
        self.regs[r] = val & MASK[r];
        if r == 13 {
            self.start_envelope();
        }
    }

    /// Port `$52`: read the chosen register back. Games test for the SGM
    /// this way, so the masks matter.
    pub fn read(&self) -> u8 {
        self.regs[self.selected as usize]
    }

    /// The programmed registers, for tools and tests.
    pub fn registers(&self) -> [u8; 16] {
        self.regs
    }

    /// A write to the shape register restarts the envelope. Shapes with
    /// bit 3 clear run once and then hold at silence, whichever way they ran.
    fn start_envelope(&mut self) {
        let shape = self.regs[13];
        self.env_attack = if shape & 4 != 0 { 0x0f } else { 0 };
        if shape & 8 == 0 {
            self.env_hold = true;
            self.env_alternate = self.env_attack != 0;
        } else {
            self.env_hold = shape & 1 != 0;
            self.env_alternate = shape & 2 != 0;
        }
        self.env_step = 15;
        self.env_count = 0;
        self.env_holding = false;
    }

    fn period(&self, ch: usize) -> u16 {
        (u16::from_le_bytes([self.regs[2 * ch], self.regs[2 * ch + 1]])).max(1)
    }

    fn env_period(&self) -> u16 {
        u16::from_le_bytes([self.regs[11], self.regs[12]]).max(1)
    }

    /// The envelope's level now, 0-15.
    fn env_level(&self) -> u8 {
        (self.env_step as u8 & 0x0f) ^ self.env_attack
    }

    fn step_envelope(&mut self) {
        if self.env_holding {
            return;
        }
        self.env_step -= 1;
        if self.env_step >= 0 {
            return;
        }
        if self.env_hold {
            if self.env_alternate {
                self.env_attack ^= 0x0f;
            }
            self.env_holding = true;
            self.env_step = 0;
        } else {
            if self.env_alternate {
                self.env_attack ^= 0x0f;
            }
            self.env_step = 15;
        }
    }

    /// One tick: 8 chip clocks, 16 CPU clocks.
    pub fn step(&mut self) {
        for ch in 0..3 {
            self.tone_count[ch] += 1;
            if self.tone_count[ch] >= self.period(ch) {
                self.tone_count[ch] = 0;
                self.tone_high[ch] = !self.tone_high[ch];
            }
        }
        self.noise_count += 1;
        if self.noise_count >= 2 * (self.regs[6] as u16).max(1) {
            self.noise_count = 0;
            let bit = (self.lfsr ^ (self.lfsr >> 3)) & 1;
            self.lfsr = (self.lfsr >> 1) | (bit << 16);
        }
        self.env_count += 1;
        if self.env_count >= 2 * self.env_period() as u32 {
            self.env_count = 0;
            self.step_envelope();
        }
    }

    /// All three channels, summed.
    pub fn output(&self) -> i16 {
        let mixer = self.regs[7];
        let noise = self.lfsr & 1 != 0;
        let mut sum = 0i32;
        for ch in 0..3 {
            let tone_on = self.tone_high[ch] || mixer & (1 << ch) != 0;
            let noise_on = noise || mixer & (8 << ch) != 0;
            if !(tone_on && noise_on) {
                continue;
            }
            let amp = self.regs[8 + ch];
            let level = if amp & 0x10 != 0 { self.env_level() } else { amp & 0x0f };
            sum += LEVEL[level as usize] as i32;
        }
        sum as i16
    }
}

impl SaveState for Ay {
    fn save(&self, w: &mut WriteCursor) {
        w.bytes(&self.regs);
        w.u8(self.selected);
        for ch in 0..3 {
            w.u16(self.tone_count[ch]);
            w.bool(self.tone_high[ch]);
        }
        w.u16(self.noise_count);
        w.u32(self.lfsr);
        w.u32(self.env_count);
        w.i8(self.env_step);
        w.u8(self.env_attack);
        w.bool(self.env_hold);
        w.bool(self.env_alternate);
        w.bool(self.env_holding);
    }

    fn load(&mut self, r: &mut ReadCursor) -> Result<(), LoadError> {
        r.bytes(&mut self.regs)?;
        self.selected = r.u8()? & 0x0f;
        for ch in 0..3 {
            self.tone_count[ch] = r.u16()?;
            self.tone_high[ch] = r.bool()?;
        }
        self.noise_count = r.u16()?;
        self.lfsr = r.u32()?;
        self.env_count = r.u32()?;
        self.env_step = r.i8()?;
        self.env_attack = r.u8()?;
        self.env_hold = r.bool()?;
        self.env_alternate = r.bool()?;
        self.env_holding = r.bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ay: &mut Ay, r: u8, v: u8) {
        ay.select(r);
        ay.write(v);
    }

    /// Every step is 3 dB below the one above it, to the nearest unit, from
    /// 8191 at the top, and the bottom is silence.
    #[test]
    fn the_level_table_is_three_db_a_step() {
        for n in 1..16 {
            let want = (8191.0 * std::f64::consts::FRAC_1_SQRT_2.powi(15 - n as i32)).round() as i16;
            assert!((LEVEL[n] - want).abs() <= 1, "step {n}: {} against {want}", LEVEL[n]);
        }
        assert_eq!(LEVEL[0], 0);
    }

    /// Registers read back masked: a game that writes $FF to a coarse tone
    /// period and reads $0F knows it found the chip.
    #[test]
    fn registers_read_back_through_their_masks() {
        let mut ay = Ay::new();
        for r in 0..16u8 {
            set(&mut ay, r, 0xff);
            assert_eq!(ay.read(), MASK[r as usize], "register {r}");
        }
        set(&mut ay, 0, 0x5a);
        ay.select(1);
        ay.select(0);
        assert_eq!(ay.read(), 0x5a);
    }

    /// Period 100 flips every 100 ticks: 200 ticks a cycle.
    #[test]
    fn a_tone_flips_every_period_ticks() {
        let mut ay = Ay::new();
        set(&mut ay, 0, 100);
        let mut flips = Vec::new();
        let mut was = ay.tone_high[0];
        for t in 1..=1000 {
            ay.step();
            if ay.tone_high[0] != was {
                flips.push(t);
                was = ay.tone_high[0];
            }
        }
        assert_eq!(flips, [100, 200, 300, 400, 500, 600, 700, 800, 900, 1000]);
    }

    /// Silence is zero: the power-on chip, and a channel with its tone on and
    /// level 0, both output nothing, not a DC level.
    #[test]
    fn silence_is_zero() {
        let mut ay = Ay::new();
        set(&mut ay, 7, 0x3e); // tone A on, the rest off
        set(&mut ay, 0, 3);
        for _ in 0..100 {
            ay.step();
            assert_eq!(ay.output(), 0);
        }
    }

    /// A channel gated by tone alone is a square wave between 0 and its level.
    #[test]
    fn a_tone_is_its_level_half_the_time() {
        let mut ay = Ay::new();
        set(&mut ay, 7, 0x3e);
        set(&mut ay, 0, 10);
        set(&mut ay, 8, 15);
        let outs: Vec<i16> = (0..200).map(|_| { ay.step(); ay.output() }).collect();
        assert_eq!(outs.iter().filter(|&&o| o == LEVEL[15]).count(), 100);
        assert_eq!(outs.iter().filter(|&&o| o == 0).count(), 100);
    }

    /// Seventeen bits tapped at 0 and 3 is maximal length: 131071 shifts.
    #[test]
    fn noise_is_maximal_length() {
        let mut ay = Ay::new();
        let start = ay.lfsr;
        let mut shifts = 0u32;
        loop {
            let before = ay.lfsr;
            ay.step();
            if ay.lfsr != before {
                shifts += 1;
                if ay.lfsr == start {
                    break;
                }
            }
            assert!(shifts <= 200_000, "never came back round");
        }
        assert_eq!(shifts, 131_071);
    }

    /// The envelope's levels, one per step, for a shape over `steps` steps.
    fn envelope(shape: u8, steps: usize) -> Vec<u8> {
        let mut ay = Ay::new();
        set(&mut ay, 11, 1); // one step every 2 ticks
        set(&mut ay, 13, shape);
        let mut v = vec![ay.env_level()];
        for _ in 1..steps {
            ay.step();
            ay.step();
            v.push(ay.env_level());
        }
        v
    }

    /// The eight distinct shapes over three ramps' worth of steps.
    #[test]
    fn the_envelope_shapes() {
        let down: Vec<u8> = (0..16).rev().collect();
        let up: Vec<u8> = (0..16).collect();
        let cat = |parts: &[&[u8]]| parts.concat();
        let zeros = [0u8; 16];
        let fifteens = [15u8; 16];
        assert_eq!(envelope(0x00, 48), cat(&[&down, &zeros, &zeros]), "\\___");
        assert_eq!(envelope(0x04, 48), cat(&[&up, &zeros, &zeros]), "/___");
        assert_eq!(envelope(0x08, 48), cat(&[&down, &down, &down]), "\\\\\\\\");
        assert_eq!(envelope(0x09, 48), cat(&[&down, &zeros, &zeros]), "\\___ held");
        assert_eq!(envelope(0x0a, 48), cat(&[&down, &up, &down]), "\\/\\/");
        assert_eq!(envelope(0x0b, 48), cat(&[&down, &fifteens, &fifteens]), "\\-- held high");
        assert_eq!(envelope(0x0c, 48), cat(&[&up, &up, &up]), "////");
        assert_eq!(envelope(0x0d, 48), cat(&[&up, &fifteens, &fifteens]), "/-- held high");
        assert_eq!(envelope(0x0e, 48), cat(&[&up, &down, &up]), "/\\/\\");
        assert_eq!(envelope(0x0f, 48), cat(&[&up, &zeros, &zeros]), "/___ held low");
    }

    #[test]
    fn state_round_trips() {
        let mut a = Ay::new();
        set(&mut a, 0, 7);
        set(&mut a, 13, 0x0e);
        for _ in 0..123 {
            a.step();
        }
        let mut w = WriteCursor::new();
        a.save(&mut w);
        let bytes = w.into_bytes();
        let mut b = Ay::new();
        let mut r = ReadCursor::new(&bytes);
        b.load(&mut r).unwrap();
        r.finish().unwrap();
        assert_eq!(a, b);
    }
}
