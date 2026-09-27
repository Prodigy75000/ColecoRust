<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (C) 2026 Prodigy75000
-->

# ColecoRust

A clean-room ColecoVision emulator core written from scratch in Rust. No C, no
bindings, no lifted code.

Started 2026-09-27. Nothing works yet.

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
| Z80 | not started |
| TMS9918A | not started |
| SN76489 | not started |
| Controllers and keypad | not started |
| Mapper / bank switching | not started |
| HLE BIOS | not started |
| Save states | not started |
| libretro | not started |
