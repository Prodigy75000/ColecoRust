// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! The libretro C ABI for ColecoRust. Not implemented yet.
//!
//! This is the layer that actually ships. Everything else in the workspace
//! links `coleco-core` directly, so without deliberate effort the shipped
//! surface is the one nothing exercises. RustStation solved that with a
//! harness that dlopens the built cdylib and drives the C ABI like a real
//! frontend; doing the same here is cheap now and expensive to retrofit.
//!
//! Two requirements that are easy to leave until too late:
//!
//! **`SET_MEMORY_MAPS` and `SYSTEM_RAM`.** Omitting them hangs
//! RetroAchievements on "waiting for core memory map". Fleet-wide rule.
//!
//! **Pixel format, declared explicitly at init.** A core that assumes the
//! frontend default gets colours subtly wrong on one platform and right on
//! another, which is miserable to chase from a screenshot.
//!
//! Deliberately empty for now. No stub exports: an `extern "C"` that returns a
//! plausible value is worse than a missing symbol, because the frontend loads
//! the core, gets an answer, and fails somewhere far away.
