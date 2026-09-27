// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The TI SN76489AN, and the resampler that turns it into a 44.1 kHz stream.
//!
//! **Derived from MegaRust's `crates/md-core/src/audio/mod.rs`** (`origin/main`
//! 3e97ca7). The tone channels, the latch/data write protocol and the save
//! layout are lifted unchanged. What changed, because the ColecoVision carries
//! TI's own part and MegaRust models the variant Sega built into its VDPs:
//!
//! - **White noise is TI's sequence, not Sega's.** Maxim's SN76489 notes
//!   (MegaRust `docs/audio/sn76489-notes.md`) give taps at bits 1 and 2 for
//!   the SC-3000H, whose chip is the same SN76489AN, against bits 0 and 3 for
//!   Sega's. His taps are stated for a 16-bit register shifting right, in
//!   which bit 0 is only a one-step delay of bit 1, so the same sequence comes
//!   from a 15-bit register tapped at bits 0 and 1: `x^15 + x + 1`, maximal
//!   length. That is what this models.
//! - **Periodic noise loops in fifteen steps**, from the register being 15
//!   bits. MegaRust's periodic-noise test names the original's loop as fifteen
//!   against Sega's sixteen; Maxim's notes do not state it for the SC-3000H,
//!   so this one is sourced weakly. It moves the pitch by about 6%.
//! - **Levels.** There is no FM chip to balance against, so a channel's full
//!   scale is a quarter of the output word rather than MegaRust's 2100.
//!
//! **One behaviour kept on no evidence either way:** a tone period of zero
//! holds the output at +1, which Maxim measured on Sega's chip. Whether TI's
//! part does the same, or counts it as 1024, is not in our sources.

use crate::save::{LoadError, ReadCursor, SaveState, WriteCursor};

/// What the frontend is handed.
pub const SAMPLE_RATE: u32 = 44_100;

/// CPU clocks per PSG step: the chip runs off the Z80's 3.579545 MHz and
/// divides by 16 internally.
pub const PSG_DIVIDER: u32 = 16;

/// Attenuation is 4 bits in 2 dB steps, 0 loudest and 15 silent: 8191 times
/// 0.794328 to the n. Four channels at full scale fill a 16-bit sample.
const VOLUME: [i16; 16] = [
    8191, 6506, 5168, 4105, 3261, 2590, 2057, 1634, 1298, 1031, 819, 651, 517, 411, 326, 0,
];

/// Fifteen-bit noise register: reset value and white-noise taps.
const LFSR_RESET: u16 = 0x4000;
const LFSR_TAPS: u16 = 0x0003;
const LFSR_TOP: u32 = 14;

/// One of the three square-wave channels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Tone {
    /// 10-bit reload value, counted in half-cycles at the divided rate.
    period: u16,
    counter: i16,
    /// Which half of the square wave is being output.
    high: bool,
    /// Attenuation, 0 loudest.
    attenuation: u8,
}

impl Tone {
    fn step(&mut self) {
        // Period zero holds the output at +1 (see the module note).
        if self.period == 0 {
            self.high = true;
            self.counter = 0;
            return;
        }
        self.counter -= 1;
        if self.counter <= 0 {
            self.counter = self.period as i16;
            self.high = !self.high;
        }
    }

    fn output(&self) -> i16 {
        let v = VOLUME[(self.attenuation & 15) as usize];
        if self.high {
            v
        } else {
            -v
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Psg {
    tone: [Tone; 3],
    /// Noise control: bit 2 selects white over periodic, bits 1-0 the rate.
    noise_ctrl: u8,
    noise_attenuation: u8,
    noise_counter: i16,
    lfsr: u16,
    /// The LFSR advances on every second expiry of the noise counter.
    noise_flip: bool,
    /// Which register the next data byte belongs to, from the last latch write.
    latched: u8,
}

impl Default for Psg {
    fn default() -> Self {
        Self::new()
    }
}

impl Psg {
    pub fn new() -> Self {
        let silent = Tone { attenuation: 15, ..Tone::default() };
        Psg {
            tone: [silent; 3],
            noise_ctrl: 0,
            // Silent at power-up. A chip that started at full volume would put
            // a click into every game's first frame.
            noise_attenuation: 15,
            noise_counter: 0,
            lfsr: LFSR_RESET,
            noise_flip: false,
            latched: 0,
        }
    }

    /// The chip has one write port and infers the destination from the byte.
    ///
    /// Bit 7 set is a LATCH: bits 6-4 pick the register and bits 3-0 are the
    /// low four bits of its value. Bit 7 clear is a DATA byte, which supplies
    /// the upper six bits of whichever tone period was latched last. Volume
    /// and noise-control registers are only four bits wide, so a data byte
    /// addressed to one of those replaces the whole value instead.
    pub fn write(&mut self, val: u8) {
        if val & 0x80 != 0 {
            self.latched = (val >> 4) & 7;
            let low = (val & 0x0f) as u16;
            match self.latched {
                0 | 2 | 4 => {
                    let ch = (self.latched / 2) as usize;
                    self.tone[ch].period = (self.tone[ch].period & 0x3f0) | low;
                }
                6 => {
                    self.noise_ctrl = val & 0x0f;
                    // Any write to the control register restarts the sequence.
                    self.lfsr = LFSR_RESET;
                }
                1 | 3 | 5 => self.tone[(self.latched / 2) as usize].attenuation = low as u8,
                _ => self.noise_attenuation = low as u8,
            }
        } else {
            let data = (val & 0x3f) as u16;
            match self.latched {
                0 | 2 | 4 => {
                    let ch = (self.latched / 2) as usize;
                    self.tone[ch].period = (self.tone[ch].period & 0x00f) | (data << 4);
                }
                6 => {
                    self.noise_ctrl = (data & 0x0f) as u8;
                    self.lfsr = LFSR_RESET;
                }
                1 | 3 | 5 => {
                    self.tone[(self.latched / 2) as usize].attenuation = (data & 0x0f) as u8
                }
                _ => self.noise_attenuation = (data & 0x0f) as u8,
            }
        }
    }

    /// Reload for the noise counter: the chip clock over 512, 1024 and 2048,
    /// or tone 3's period, which is how games sweep the noise pitch.
    fn noise_reload(&self) -> i16 {
        match self.noise_ctrl & 3 {
            0 => 32,
            1 => 64,
            2 => 128,
            _ => self.tone[2].period.max(1) as i16,
        }
    }

    fn step_noise(&mut self) {
        self.noise_counter -= 1;
        if self.noise_counter > 0 {
            return;
        }
        self.noise_counter = self.noise_reload();
        self.noise_flip = !self.noise_flip;
        if !self.noise_flip {
            return;
        }
        let feedback = if self.noise_ctrl & 4 != 0 {
            (self.lfsr & LFSR_TAPS).count_ones() as u16 & 1
        } else {
            self.lfsr & 1
        };
        self.lfsr = (self.lfsr >> 1) | (feedback << LFSR_TOP);
    }

    /// Advance one step of the internally divided clock.
    pub fn step(&mut self) {
        for t in &mut self.tone {
            t.step();
        }
        self.step_noise();
    }

    /// The chip's programmed registers, without its running counters: tone
    /// periods and attenuations, then noise control and attenuation, then the
    /// latched register. What a routine sets, independent of when it ran.
    pub fn registers(&self) -> [u16; 9] {
        let t = &self.tone;
        [
            t[0].period,
            t[0].attenuation as u16,
            t[1].period,
            t[1].attenuation as u16,
            t[2].period,
            t[2].attenuation as u16,
            self.noise_ctrl as u16,
            self.noise_attenuation as u16,
            self.latched as u16,
        ]
    }

    /// Mono output of all four channels.
    pub fn output(&self) -> i16 {
        let mut sum: i32 = self.tone.iter().map(|t| t.output() as i32).sum();
        let nv = VOLUME[(self.noise_attenuation & 15) as usize] as i32;
        sum += if self.lfsr & 1 != 0 { nv } else { -nv };
        sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }
}

impl SaveState for Psg {
    fn save(&self, w: &mut WriteCursor) {
        for t in &self.tone {
            w.u16(t.period);
            w.i16(t.counter);
            w.bool(t.high);
            w.u8(t.attenuation);
        }
        w.u8(self.noise_ctrl);
        w.u8(self.noise_attenuation);
        w.i16(self.noise_counter);
        w.u16(self.lfsr);
        w.bool(self.noise_flip);
        w.u8(self.latched);
    }

    fn load(&mut self, r: &mut ReadCursor) -> Result<(), LoadError> {
        for i in 0..3 {
            self.tone[i].period = r.u16()?;
            self.tone[i].counter = r.i16()?;
            self.tone[i].high = r.bool()?;
            self.tone[i].attenuation = r.u8()?;
        }
        self.noise_ctrl = r.u8()?;
        self.noise_attenuation = r.u8()?;
        self.noise_counter = r.i16()?;
        self.lfsr = r.u16()?;
        self.noise_flip = r.bool()?;
        self.latched = r.u8()?;
        Ok(())
    }
}

/// The PSG plus a box-filter resampler driven by integer counters.
///
/// Every CPU clock adds `SAMPLE_RATE * PSG_DIVIDER` to a numerator and a
/// sample is due each time it passes `CPU_HZ * PSG_DIVIDER`, with the
/// remainder carried, so a second of emulated time yields exactly
/// `SAMPLE_RATE` samples and no float enters the saved state. Same scheme as
/// MegaRust's `Audio`, for the same save-state reason.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Audio {
    pub psg: Psg,
    /// CPU clocks not yet consumed by a PSG step.
    step_accum: u32,
    /// Sample-clock numerator.
    sample_accum: u64,
    sum: i64,
    count: u32,
    /// Samples produced and not yet taken. Not saved: a frontend drains it
    /// every frame, and a restore starts a fresh buffer.
    pub out: Vec<i16>,
}

impl Default for Audio {
    fn default() -> Self {
        Self::new()
    }
}

impl Audio {
    pub fn new() -> Self {
        Audio { psg: Psg::new(), step_accum: 0, sample_accum: 0, sum: 0, count: 0, out: Vec::new() }
    }

    /// Advance by `cycles` CPU clocks.
    pub fn run(&mut self, cycles: u32) {
        self.step_accum += cycles;
        while self.step_accum >= PSG_DIVIDER {
            self.step_accum -= PSG_DIVIDER;
            self.psg.step();
            self.sum += self.psg.output() as i64;
            self.count += 1;
            self.sample_accum += SAMPLE_RATE as u64 * PSG_DIVIDER as u64;
            let due = crate::CPU_HZ as u64;
            if self.sample_accum >= due {
                self.sample_accum -= due;
                self.out.push((self.sum / self.count.max(1) as i64) as i16);
                self.sum = 0;
                self.count = 0;
            }
        }
    }
}

impl SaveState for Audio {
    fn save(&self, w: &mut WriteCursor) {
        self.psg.save(w);
        w.u32(self.step_accum);
        w.u64(self.sample_accum);
        w.i64(self.sum);
        w.u32(self.count);
    }

    fn load(&mut self, r: &mut ReadCursor) -> Result<(), LoadError> {
        self.psg.load(r)?;
        self.step_accum = r.u32()?;
        self.sample_accum = r.u64()?;
        self.sum = r.i64()?;
        self.count = r.u32()?;
        self.out.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry is the 2 dB law it claims to be, to the nearest unit, and
    /// the last one is true silence.
    #[test]
    fn the_volume_table_is_two_db_a_step() {
        for (n, &v) in VOLUME[..15].iter().enumerate() {
            let want = (8191.0 * 0.794_328_f64.powi(n as i32)).round() as i16;
            assert!((v - want).abs() <= 1, "step {n}: {v} against {want}");
        }
        assert_eq!(VOLUME[15], 0);
    }

    /// Periodic noise is a ring counter. On TI's part it comes back round in
    /// fifteen shifts; Sega's takes sixteen, which is what MegaRust tests.
    #[test]
    fn periodic_noise_repeats_every_fifteen_shifts() {
        let mut psg = Psg::new();
        psg.write(0xe0); // noise control: periodic, fastest rate
        let start = psg.lfsr;
        let mut shifts = 0;
        loop {
            let before = psg.lfsr;
            psg.step_noise();
            if psg.lfsr != before {
                shifts += 1;
                if psg.lfsr == start {
                    break;
                }
            }
            assert!(shifts <= 64, "the sequence never came back round");
        }
        assert_eq!(shifts, 15);
    }

    /// White noise from a 15-bit register tapped at bits 0 and 1 is a
    /// maximal-length sequence: 32767 states before it repeats. Tapping bits 1
    /// and 2 of a 15-bit register, a plausible misreading of Maxim's notes,
    /// fails this.
    #[test]
    fn white_noise_is_maximal_length() {
        let mut psg = Psg::new();
        psg.write(0xe4); // white, fastest rate
        let start = psg.lfsr;
        let mut shifts = 0u32;
        loop {
            let before = psg.lfsr;
            psg.step_noise();
            if psg.lfsr != before {
                shifts += 1;
                if psg.lfsr == start {
                    break;
                }
            }
            assert!(shifts <= 70_000, "never came back round");
        }
        assert_eq!(shifts, 32_767);
    }

    #[test]
    fn silence_at_power_up_is_zero_not_a_dc_level() {
        let mut a = Audio::new();
        a.run(crate::CPU_HZ / 10);
        assert!(!a.out.is_empty());
        assert!(a.out.iter().all(|&s| s == 0));
    }

    /// The sample count never drifts from the declared rate by more than the
    /// one sample that can be pending inside an unfinished PSG step, however
    /// long the run and however unevenly it is fed. (Exactly 44,100 at one
    /// second is not the property: 3,579,545 clocks is 223,721 whole steps
    /// and 9 clocks, and the last sample falls due in those 9.)
    #[test]
    fn the_resampler_does_not_drift_from_the_declared_rate() {
        let mut a = Audio::new();
        let mut produced = 0u64;
        let mut chunk = 1;
        for second in 1..=10u64 {
            let mut left = crate::CPU_HZ;
            while left > 0 {
                let c = chunk.min(left);
                a.run(c);
                left -= c;
                chunk = chunk % 23 + 4;
            }
            produced += a.out.len() as u64;
            a.out.clear();
            let want = second * SAMPLE_RATE as u64;
            assert!(want.abs_diff(produced) <= 1,"after {second} s: {produced} against {want}");
        }
    }

    #[test]
    fn a_loud_tone_is_heard() {
        let mut a = Audio::new();
        a.psg.write(0x80 | 0x0e); // tone 1 period low bits
        a.psg.write(0x0f); // period 0xfe, about 440 Hz
        a.psg.write(0x90); // tone 1 attenuation 0
        a.run(crate::CPU_HZ / 10);
        let peak = a.out.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 8000, "peak {peak}");
    }
}
