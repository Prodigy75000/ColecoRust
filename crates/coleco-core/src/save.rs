// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! Byte-identical save-state backbone.
//!
//! Netplay across two engines (and rewind, and cross-device transfer) is only
//! sound if a save state is *byte-for-byte deterministic*: the same machine
//! state must always serialize to the same bytes, on every platform, forever.
//! We get that by construction rather than by luck:
//!
//!   * Every primitive is written **little-endian, fixed-width**. No host
//!     endianness, no `usize` (whose width differs 32- vs 64-bit), no pointer
//!     values, no floats in the machine state. Note the endianness of the
//!     *state stream* is unrelated to the 68000's big-endian bus: the bus layer
//!     owns byte order, and state always goes out little-endian.
//!   * Every aggregate writes its fields in a **fixed, documented order**; there
//!     are no hash-map iterations or other nondeterministic orderings anywhere
//!     in the serialized set.
//!   * Load is the exact structural inverse of save. A round-trip
//!     (`save` then `load` then `save`) is asserted byte-identical in tests.
//!
//! ColecoRust took this file unchanged from MegaRust (`origin/main` 3e97ca7,
//! 2026-09-27), so the note below is MegaRust's and still true of the lineage.
//!
//! This cursor is a straight lift of FamiRust's `save.rs`, which the contract in
//! `docs/SAVESTATE.md` names as the reference implementation. Keep it that way:
//! a divergence here is a divergence in the cross-core format.
//!
//! **Multi-instance note.** This family grows extra CPUs (a second 68000 for the
//! Sega CD sub-CPU, two SH-2s for the 32X). Each instance serializes its own
//! full field set, in a fixed instance order, so adding one is a
//! `format_version` bump rather than a redesign.

/// Append-only little-endian writer over an owned byte buffer.
#[derive(Default)]
pub struct WriteCursor {
    buf: Vec<u8>,
}

impl WriteCursor {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }

    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    /// Raw byte block (work RAM, VRAM, CRAM, VSRAM, sound RAM, cart SRAM).
    /// Length is *not* prefixed: both sides agree on the fixed size, which keeps
    /// states compact and makes a size mismatch a load-time error rather than
    /// silent misalignment.
    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }
}

/// Error returned when a state buffer is malformed or truncated. A cross-engine
/// transfer must *refuse* rather than load a partial/garbage state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// Ran off the end of the buffer while reading.
    UnexpectedEof,
    /// A tagged field carried a value outside its legal domain.
    BadValue(&'static str),
    /// Trailing bytes remained after a full structural read (state longer than
    /// the machine expects), a version/shape mismatch we refuse up front.
    TrailingBytes,
    /// Leading magic did not match this core: not a state from this core at all.
    BadMagic,
    /// State `format_version` is newer than this build understands. Per the
    /// save-state contract we refuse cleanly rather than misread a layout we do
    /// not know (the carried value is the version we found).
    UnsupportedVersion(u16),
}

/// Sequential little-endian reader; the exact inverse of [`WriteCursor`].
pub struct ReadCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ReadCursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Assert the whole buffer was consumed. Call once at the top-level load.
    pub fn finish(&self) -> Result<(), LoadError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(LoadError::TrailingBytes)
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], LoadError> {
        let end = self.pos.checked_add(n).ok_or(LoadError::UnexpectedEof)?;
        if end > self.buf.len() {
            return Err(LoadError::UnexpectedEof);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, LoadError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, LoadError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, LoadError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, LoadError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i8(&mut self) -> Result<i8, LoadError> {
        Ok(self.take(1)?[0] as i8)
    }

    pub fn i16(&mut self) -> Result<i16, LoadError> {
        Ok(i16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, LoadError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, LoadError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bool(&mut self) -> Result<bool, LoadError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(LoadError::BadValue("bool")),
        }
    }

    /// Read exactly `dst.len()` bytes into `dst` (the fixed-size inverse of
    /// [`WriteCursor::bytes`]).
    pub fn bytes(&mut self, dst: &mut [u8]) -> Result<(), LoadError> {
        let n = dst.len();
        dst.copy_from_slice(self.take(n)?);
        Ok(())
    }

    /// Advance the cursor past `n` bytes without reading them.
    pub fn skip(&mut self, n: usize) -> Result<(), LoadError> {
        self.take(n)?;
        Ok(())
    }
}

/// Every serializable subsystem implements this. `save` and `load` must visit
/// exactly the same fields in exactly the same order.
pub trait SaveState {
    fn save(&self, w: &mut WriteCursor);
    fn load(&mut self, r: &mut ReadCursor) -> Result<(), LoadError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip_little_endian() {
        let mut w = WriteCursor::new();
        w.u8(0x12);
        w.u16(0x3456);
        w.u32(0x789a_bcde);
        w.u64(0x0102_0304_0506_0708);
        w.i16(-2);
        w.bool(true);
        w.bytes(&[0xde, 0xad, 0xbe, 0xef]);
        let bytes = w.into_bytes();

        // Little-endian layout is part of the on-disk contract, and stays LE even
        // though the emulated bus is big-endian.
        assert_eq!(&bytes[0..1], &[0x12]);
        assert_eq!(&bytes[1..3], &[0x56, 0x34]);
        assert_eq!(&bytes[3..7], &[0xde, 0xbc, 0x9a, 0x78]);

        let mut r = ReadCursor::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0x12);
        assert_eq!(r.u16().unwrap(), 0x3456);
        assert_eq!(r.u32().unwrap(), 0x789a_bcde);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.i16().unwrap(), -2);
        assert!(r.bool().unwrap());
        let mut blk = [0u8; 4];
        r.bytes(&mut blk).unwrap();
        assert_eq!(blk, [0xde, 0xad, 0xbe, 0xef]);
        r.finish().unwrap();
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        let bytes = [0x01u8, 0x02];
        let mut r = ReadCursor::new(&bytes);
        assert_eq!(r.u32(), Err(LoadError::UnexpectedEof));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let bytes = [0x01u8, 0x02];
        let mut r = ReadCursor::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0x01);
        assert_eq!(r.finish(), Err(LoadError::TrailingBytes));
    }

    #[test]
    fn non_canonical_bool_is_rejected() {
        let bytes = [0x02u8];
        let mut r = ReadCursor::new(&bytes);
        assert_eq!(r.bool(), Err(LoadError::BadValue("bool")));
    }
}
