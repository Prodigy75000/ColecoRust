// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Prodigy75000

//! Stamps a build identity into the core, so a binary can say which commit it
//! came from: `strings -a libcolecorust_libretro.so | grep build=`.
//!
//! `jniLibs/` in the Android app is a shared directory with no version in it,
//! written by several core repos and assembled by a third. Without a stamp,
//! "which ColecoRust is on this device" is answered from mtimes and section sizes,
//! which is inference. Lifted from MegaRust, which took it from PocketRust, so
//! every one of my cores is read the same way.
//!
//! The identity is passed IN by `scripts/deploy-android-debug.sh` as
//! `COLECORUST_BUILD_ID` and declared a rerun trigger, because a build script's
//! output is cached and a hash computed here could survive into a later build
//! and name the wrong commit. Reading git here is only a fallback for an
//! ordinary `cargo build`, suffixed `-local` so it cannot pass for a deploy.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=COLECORUST_BUILD_ID");
    watch_git_head();

    let id = std::env::var("COLECORUST_BUILD_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(fallback_id);
    // Something `strings | grep` can pick out of a stripped .so, and nothing
    // that could break that.
    let id: String = id
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .take(32)
        .collect();
    let id = if id.is_empty() { "unknown".into() } else { id };
    println!("cargo:rustc-env=COLECORUST_BUILD_ID={id}");
}

/// Re-run when the checked-out commit moves. `.git/HEAD` only changes with the
/// branch, so the ref it points at is watched too.
fn watch_git_head() {
    let Some(manifest) = std::env::var("CARGO_MANIFEST_DIR").ok().map(std::path::PathBuf::from) else {
        return;
    };
    let Some(git) = manifest.ancestors().map(|a| a.join(".git")).find(|p| p.is_dir()) else {
        return;
    };
    let head = git.join("HEAD");
    if !head.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(r) = contents.strip_prefix("ref:").map(str::trim) {
            let path = git.join(r);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

fn fallback_id() -> String {
    Command::new("git")
        .args(["rev-parse", "--short=9", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|h| format!("{h}-local"))
        .unwrap_or_else(|| "unknown".into())
}
