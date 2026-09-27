<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (C) 2026 Prodigy75000
-->

# ColecoRust

A clean-room ColecoVision emulator core written in Rust. No C, no bindings, and
no third-party emulator code. It does reuse the owner's own in-house cores:
the Z80 is MegaRust's, and the VDP and PSG are adapted from MegaRust's. Each
such file says where it came from and at which commit.

Started 2026-09-27. With NO BIOS file, on ColecoRust's own HLE BIOS, 132 of the
collection's 164 commercial titles reach gameplay (151 on the real BIOS).

## The point of this core is the HLE BIOS

Most of this machine is small and well documented. That is not what makes the
project interesting. The objective is that **a user brings a cartridge and
nothing else**, on the majority of commercial titles, with no BIOS dump
anywhere.

Needing a firmware file has always been a drag, and it is worst exactly here:
these are eight-minute games from 1982 that somebody wants to play on a phone
in under a minute, and the setup currently costs more than the session does.
RustStation proved the approach on a far larger machine, booting every disc
tried so far on its own kernel with no BIOS file present.

ColecoVision is a good place to do this properly because the surface is small
enough to be finished rather than approximated, and because there is a real
corpus to be judged against rather than a handful of demos.

## The machine

| Part | What it is |
|---|---|
| CPU | Zilog Z80A at 3.579545 MHz |
| Video | TI TMS9918A, 256x192, 16 colours, 32 sprites, 16 KB of its own VRAM |
| Sound | TI SN76489AN, three square channels and a noise channel |
| RAM | 1 KB, which is not a typo |
| BIOS | 8 KB at 0x0000, containing the boot screen, the controller decoder and the sound driver |
| Cartridge | 8 KB to 32 KB in the common case, with bank switching beyond that |
| Controllers | two ports, each a joystick, two fire buttons and a 12-key keypad |

## Why the BIOS is the hard part rather than the easy part

It is tempting to read an 8 KB ROM as a formality. It is not, because games do
not merely boot through it, they **keep calling it**:

- the controller decoder lives in the BIOS, and the keypad in particular is
  read through it rather than off the port directly,
- the sound driver lives in the BIOS, and games hand it data structures rather
  than writing the SN76489 themselves,
- games depend on RAM the BIOS maintains, so the HLE has to leave the same
  bytes behind in the same places,
- the boot screen is not decoration: its timing is observable, and some titles
  lean on it.

So the HLE is not "skip the logo". It is a reimplementation of a published
interface, with the corpus as the judge.

## How correctness gets decided

Trial and error against a real corpus is the method the owner asked for, and it
is the right one here, but it needs a rule or it becomes an opinion:

- **A real BIOS is the oracle, never a requirement.** Any dump in `bios/` is a
  development tool. Run a title both ways and diff. When HLE and the real BIOS
  disagree, the real one is right until proven otherwise.
- **A verdict is a ledger row, not a memory.** Per-title, recorded, with what
  was observed. The other in-house cores learned this the expensive way: a
  free-text compatibility note accumulated 24 spellings for the same fault
  before anyone noticed it could not be grouped.
- **Tests must be able to fail.** Prove a new test fails before trusting it.

## Layout

    crates/coleco-core       the machine: Z80, TMS9918A, SN76489, mapper, HLE BIOS
    crates/coleco-libretro   the shipped cdylib, the libretro C ABI
    crates/coleco-runner     dev harnesses: test ROMs, headless renders, HLE-vs-BIOS diffs
    docs/notes               our own design notes and measurements
    docs/ref                 derived reference write-ups that cite their sources

## House rules inherited from the other in-house cores

- **Save states are a byte-identical contract.** Fixed size, little-endian,
  versioned, identical on x86-64 and arm64. Binding across every in-house core.
- **`SET_MEMORY_MAPS` and `SYSTEM_RAM` are not optional.** A libretro core that
  omits them hangs RetroAchievements on "waiting for core memory map".
- **No third-party documents in git.** Only markdown under `docs/`, and only our
  own notes plus derived write-ups that cite their sources.
- **Cartridge images and BIOS dumps never enter the repository.**

## Status

| Component | State |
|-----------|-------|
| Z80 | lifted from MegaRust (`origin/main` 3e97ca7), ZEXDOC and ZEXALL 79/79 here; `cargo run --release -p coleco-runner --bin zexall` |
| TMS9918A | adapted from MegaRust's SG-1000 path to TI's chip (3-bit register select, Graphics II masks, no mode 4) |
| SN76489 | adapted from MegaRust to TI's SN76489AN (15-bit noise register); 44.1 kHz mono |
| Controllers and keypad | joystick and keypad modes; all twelve keypad codes checked against the real BIOS decoding table (`*` and `#` were swapped, fixed) |
| Mapper / bank switching | not started |
| HLE BIOS | image, own font, hand-over boot, and 22 routines: VDP registers and VRAM, tables, sprites, font, RNG, MODE_1, controllers and keypad, and the sound driver (including game-supplied special sounds, which call back into the cartridge). **132 of 164 commercial titles ALIVE with no BIOS file** (151 on the real BIOS); `ledger/smoke-hle.tsv` |
| Save states | v1 layout, fixed size, round-trip tested |
| libretro | `libcolecorust_libretro`: always the HLE BIOS (never asks for coleco.rom), 256x192 XRGB8888, 44.1 kHz, save states, SYSTEM_RAM and SET_MEMORY_MAPS, keypad in Gearcoleco's scheme, which Trophy Hub's on-screen keypad sends (Y X L R = 1-4, L2 R2 L3 R3 = 5-8, left stick Y/X = 9/0, START = *, SELECT = #), and a keyboard. Builds for arm64 Android; `scripts/deploy-android-debug.sh` |
| Corpus smoke, REAL BIOS | 151 of 164 commercial titles and 42 of 48 PD titles ALIVE, 0 crashed; `ledger/smoke-real-bios.tsv`, `cargo run --release -p coleco-runner --bin smoke` |
