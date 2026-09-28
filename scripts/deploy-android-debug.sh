#!/usr/bin/env bash
#
# One-liner: build the arm64 ColecoRust libretro core from the current working
# tree, drop it into the TrophyHubAndroid debug app's jniLibs, and assemble the
# debug APK (optionally install it to an attached device).
#
#   scripts/deploy-android-debug.sh            # build core -> jniLibs -> assembleDebug
#   scripts/deploy-android-debug.sh install    # ... then adb install -r the APK
#
# Why this exists: the core (ColecoRust) and the app (TrophyHubAndroid) are
# separate repos in the TrophyHub umbrella, so getting a core change onto a
# device is a fixed three-step dance. This pins it so no one rediscovers it.
# Lifted from MegaRust's script of the same name (itself from SuperRust's); only
# the crate, the .so name and the notes below differ. Only arm64-v8a ships (see the app's abiFilters);
# the two devices are arm64.
#
# NOTE ON THE BIOS: there is none. The core always runs its own HLE BIOS and
# never asks the frontend for coleco.rom. A device with a BIOS installed runs
# exactly the same code as one without.
#
# NOTE ON SAVE STATES: SAVE_STATE_VERSION in crates/coleco-core/src/lib.rs (v2
# as of 2026-09-27). A bump means states from an older build are refused, not
# misread; roll both devices together before a netplay session.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ANDROID="$(cd "$REPO/../TrophyHubAndroid" && pwd)"
TRIPLE="aarch64-linux-android"
ABI="arm64-v8a"
SO="libcolecorust_libretro.so"
# ColecoRust is a standalone workspace and builds into its own target/, unlike
# SuperRust which shares the umbrella's at TrophyHub/target. Check both, own
# first, so this keeps working whichever way CARGO_TARGET_DIR is set.
# The commit this core is built from, stamped into the .so (see
# crates/coleco-libretro/build.rs) so a device's core can be named with a grep.
# Computed fresh every run and exported, which also forces the build script to
# re-run. -dirty when the tree has uncommitted changes, because then the hash
# names where the build started and not what it contains.
COLECORUST_BUILD_ID="$(git -C "$REPO" rev-parse --short=9 HEAD)"
if [ -n "$(git -C "$REPO" status --porcelain --untracked-files=no)" ]; then
  COLECORUST_BUILD_ID="$COLECORUST_BUILD_ID-dirty"
fi
export COLECORUST_BUILD_ID
echo "[1/3] cargo build --release -p coleco-libretro --target $TRIPLE  (build=$COLECORUST_BUILD_ID)"
( cd "$REPO" && cargo build --release -p coleco-libretro --target "$TRIPLE" )

SRC=""
for dir in "${CARGO_TARGET_DIR:-}" "$REPO/target" "$REPO/../target"; do
  [ -n "$dir" ] || continue
  if [ -f "$dir/$TRIPLE/release/$SO" ]; then
    SRC="$dir/$TRIPLE/release/$SO"
    break
  fi
done
DST="$ANDROID/app/src/main/jniLibs/$ABI/$SO"
[ -n "$SRC" ] || { echo "ERROR: built core not found in any target dir" >&2; exit 1; }
mkdir -p "$(dirname "$DST")"
echo "[2/3] cp $SRC -> $DST"
cp "$SRC" "$DST"
# Check the copy, not the build: the point is what jniLibs now holds.
if ! grep -aq "build=$COLECORUST_BUILD_ID" "$DST"; then
  echo "ERROR: $DST does not carry build=$COLECORUST_BUILD_ID" >&2
  exit 1
fi
echo "      stamped build=$COLECORUST_BUILD_ID"

echo "[3/3] ./gradlew :app:assembleDebug"
( cd "$ANDROID" && ./gradlew :app:assembleDebug )

APK="$ANDROID/app/build/outputs/apk/debug/app-debug.apk"
echo "APK: $APK"

# The debug APK goes to a Drive slot of this core's own, replacing the last one
# there, so a phone/tablet can pull it without a cable (Drive for Desktop syncs
# it up). Not the shared TrophyHub-debug.apk: that one has a notes file beside it
# naming its build, and overwriting the APK from here leaves the notes describing
# a different one. Set TROPHYHUB_DEBUG_APK to aim elsewhere.
DRIVE_SLOT="${TROPHYHUB_DEBUG_APK:-/g/My Drive/Trophy Hub/TrophyHub-debug-colecorust.apk}"
if [ -d "$(dirname "$DRIVE_SLOT")" ]; then
  cp "$APK" "$DRIVE_SLOT"
  echo "Drive: $DRIVE_SLOT (replaced)"
else
  echo "note: Drive slot dir missing, skipped ($DRIVE_SLOT)"
fi

if [ "${1:-}" = "install" ]; then
  echo "adb install -r (device must be authorized for USB debugging)"
  adb install -r "$APK"
fi
