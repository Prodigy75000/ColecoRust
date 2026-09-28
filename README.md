<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (C) 2026 Prodigy75000
-->

# ColecoRust

A ColecoVision emulator core written in Rust, with a BIOS of its own: games run
from the cartridge alone, with no BIOS file to find. No C, no bindings, and no
third-party emulator code. The Z80 comes from MegaRust, my Sega emulator, and
the video and sound chips started from its code too; each such file says where
it came from and at which commit.

## Status

| Component | State |
|-----------|-------|
| CPU (Z80A, 3.58 MHz) | ✅ from MegaRust; ZEXDOC and ZEXALL pass |
| Video (TMS9918A) | ✅ Graphics I and II, text and multicolour modes, 32 sprites with the four-per-line limit, collision and fifth-sprite flags |
| Sound (SN76489AN) | ✅ three tone channels and noise, TI's 15-bit noise register; 44.1 kHz |
| Super Game Module | ✅ its 32 KB of RAM and AY-3-8910 sound chip, always plugged in |
| Controllers | ✅ joystick, two fire buttons and the 12-key keypad, both ports |
| BIOS | ✅ its own, see below; the real one is never needed |
| Save states | ✅ full machine state; a state reloads exactly as it was saved |
| Memory map | ✅ SYSTEM_RAM and a memory descriptor, so achievements, cheats and RAM watch all address the core |
| Mega Cart bank switching | ✅ up to 1 MB; the other boards over 32 KB (the 64 KB ones with the header up front) do not load yet |
| Speed rollers and steering wheel | ❌ not yet: the Super Action controllers and the Expansion Module 2 wheel have no input |
| PAL | ❌ NTSC only |

Compatibility: **153 of 164** commercial ColecoVision titles reach gameplay with
no BIOS file, in a headless smoke test over the full collection that plays each
one for 25 seconds and presses 1 and fire along the way. On the real BIOS the
same test gets 151. Every title that reaches gameplay on the real BIOS does so
here; the two more are Looping and Tournament Tennis, which play on both and
simply sit on a still screen at the moment the test looks on the real one.

The 11 that do not reach gameplay fail the same test on the real BIOS too: two test
cartridges, a demo, and games the test's key presses do not get
past (Dr. Seuss's Fix-Up the Mix-Up Puzzler, Facemaker, Gateway to Apshai,
Moonsweeper, Nova Blast, Omega Race, Telly Turtle, The Yolk's on You). Of 48
public-domain and homebrew titles, 45 reach gameplay, the same as on the real
BIOS.

All 28 Mega Cart homebrew titles tried reach their games, the 21 of them made
for the Super Game Module included (Knightmare, Gauntlet, Wizard of Wor, the
Super Game editions of Zaxxon, Subroc and Buck Rogers, and more). The module
changes nothing for the original library: the full smoke test gives the same
result, title for title, with it plugged in.

## The BIOS

The ColecoVision's 8 KB BIOS is not only a boot screen. Games call into it all
the time: the sound driver, the controller and keypad decoding, the sprite and
background object system, timers, and the routines that write the video chip.
ColecoRust replaces all of it with Rust: every one of the 53 routines in the
BIOS's jump table, the forms that take their parameters from after the call,
and the handful of addresses inside the BIOS that some games call directly.

Each routine is checked against the real BIOS on every call the game collection
makes: the same RAM, video memory and registers afterwards, and the same time
taken, to the cycle for most. The font is its own, and the title screen is
skipped, so games start at once.

A real BIOS dump is only ever a development tool, to compare against.

## Layout

```
crates/
  coleco-core/      the emulator library (no I/O deps)
    src/z80/        the CPU
    src/{vdp, psg, ay, machine, save}.rs
    src/hle/        the BIOS: routines, sound driver, objects, timers, transforms
  coleco-libretro/  libretro core (the .so / .dll for libretro frontends)
  coleco-runner/    headless tools: renders, the smoke test, BIOS comparisons, ZEXALL
ledger/             smoke test results, per title
```

## Running

```sh
# Run a cartridge for 600 frames and save the last one as a PNG
cargo run --release -p coleco-runner --bin coleco -- --frames 600 --png out.png game.col

# Press keypad 1 at frame 300, fire at 400
cargo run --release -p coleco-runner --bin coleco -- --frames 600 --key 300:1 --fire 400 --png out.png game.col

# Smoke-test a folder of cartridges; writes a ledger and contact sheets
cargo run --release -p coleco-runner --bin smoke -- --hle --out out/smoke path/to/roms/

# The CPU test suite, and the unit tests
cargo run --release -p coleco-runner --bin zexall
cargo test
```

With a real BIOS at hand, two more tools compare against it: `routinediff`
replays every call the collection makes to one BIOS routine (or all of them)
on both and reports any difference, and `lockstep` runs one game on both side
by side until they first disagree.

## libretro core

`coleco-libretro` is a standard libretro core. It never asks the frontend for
a BIOS file.

```sh
cargo build --release -p coleco-libretro
# -> target/release/{libcolecorust_libretro.so | colecorust_libretro.dll | libcolecorust_libretro.dylib}
```

For Android, add the target and point Cargo at your NDK's linker in
`.cargo/config.toml`:

```sh
rustup target add aarch64-linux-android
cargo build --release -p coleco-libretro --target aarch64-linux-android
# -> target/aarch64-linux-android/release/libcolecorust_libretro.so
```

The core stamps the commit it was built from into the library:

```sh
strings -a libcolecorust_libretro.so | grep 'COLECORUST build='
```

Video is 256x192 XRGB8888, audio 44.1 kHz stereo. The keypad follows
Gearcoleco's layout, so on-screen keypads made for that core work unchanged:

| ColecoVision | RetroPad | Keyboard (player 1) |
|---|---|---|
| joystick | D-pad | |
| left fire, right fire | B, A | |
| keypad 1, 2, 3, 4 | Y, X, L, R | 1 to 4 |
| keypad 5, 6, 7, 8 | L2, R2, L3, R3 | 5 to 8 |
| keypad 9, 0 | left stick down, left stick right | 9, 0 |
| keypad `*`, `#` | Start, Select | `*`, `#` |

## License

GNU General Public License v3.0 or later. See [LICENSE](LICENSE).
