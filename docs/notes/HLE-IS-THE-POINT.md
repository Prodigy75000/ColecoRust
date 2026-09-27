# HLE is the point, and how it gets judged

Written 2026-09-27, before any emulation existed, by the umbrella orchestrator
scaffolding the repo. Everything here is either the owner's stated objective or
a question flagged open for this core's agent. Nothing here is a measurement,
because there is nothing yet to measure.

## The objective, in the owner's terms

Run the **majority of commercial titles with no BIOS dump present**. Requiring a
firmware file has always been a drag, and it is worst on machines like this one:
these are short games from 1982 that somebody wants to play on a phone in under
a minute, and today the setup costs more than the session does.

The owner also named the method: **trial and error against the corpus**. That is
the right method here. The BIOS interface is published, the corpus is finite,
and the feedback loop is a game either working or not. What it needs is a rule
so it stays evidence rather than opinion, which is the section below.

This is the first of three. Odyssey2 and Intellivision are intended to follow as
their own repos, because the three machines share no silicon: Z80 against Intel
8048 against CP1610, with three unrelated video and sound parts. What travels
between them is the HLE METHOD, not code.

## Why the BIOS is the hard part rather than a formality

Eight kilobytes reads like something to skip. It is not, because games do not
merely boot through it, they keep calling it:

- **The controller decoder is in the BIOS.** The keypad in particular is read
  through BIOS routines rather than off the port directly.
- **The sound driver is in the BIOS.** Games hand it data structures rather than
  writing the SN76489 themselves, so an HLE that models the chip perfectly and
  not the driver produces silence.
- **Games depend on RAM the BIOS maintains.** The HLE has to leave the same
  bytes in the same places. With 1 KB of work RAM there is nowhere to hide
  bookkeeping either: put exactly what the real BIOS puts where it puts it, and
  keep everything else host-side.
- **The boot screen is observable.** Its timing is not decoration and some
  titles lean on it.

So this is a reimplementation of a published interface, and the corpus is the
judge.

## How a verdict is decided, so trial and error stays evidence

**A real BIOS is the oracle, never a requirement.** Any dump in `bios/` is a
development tool. Run a title both ways and diff. When HLE and the real BIOS
disagree, the real one is right until proven otherwise. A core that quietly
starts needing the dump has failed at its only real objective.

**A verdict is a ledger row, not a memory.** Per-title, recorded, with what was
observed. The other in-house cores learned this expensively: a free-text
compatibility column accumulated 24 different spellings for the same fault
before anyone noticed it could not be grouped, which turned a work queue into
prose. If a ledger appears here, give it a fixed vocabulary from the start and
derive the verdict rather than typing it.

**Tests must be able to fail.** Prove a new test fails before trusting it. This
is a fleet rule and it matters more than usual on a core whose method is trial
and error, because "it boots now" is exactly the kind of evidence that rots.

**"Majority of commercial titles" needs a denominator before it means
anything.** Decide what the corpus is and how many titles are in it early, and
state coverage as a fraction of that. The owner's collection is at
`TrophyHubResources/emulator-resources/rom-collections/ColecoVision.7z`, outside
this repo.

## Open questions, deliberately not decided here

Scaffolding a repo is not designing a core, and an orchestrator guessing at
these would be inventing constraints the owner did not set.

**Where the HLE intercepts.** Trapping calls at BIOS entry addresses, providing
a synthetic 8 KB image whose entry points are jumps into host code, or
something else. Each has different consequences for save states and for
cartridges that read the BIOS area as data.

**What a cartridge that reads BIOS bytes should see.** Some titles read the
firmware region for reasons other than calling it. An HLE that presents an empty
window will be caught out by those, and how often that happens is a question for
the corpus rather than for reasoning.

**Mapper coverage.** Which bank-switching schemes actually appear in the corpus,
as opposed to which ones exist.

**Whether the boot delay is emulated, skipped, or configurable.** It is
observable, it is also the thing that makes the machine annoying to use, and
those two facts pull in opposite directions. This one is a user-experience
decision as much as a technical one, so it is worth putting to the owner rather
than settling privately.

> **DECIDED 2026-09-27 by the owner: SKIPPED.** In his words, the emulation
> community prides itself on boot skip, it is the best user experience, and
> losing the legitimate boot is far outweighed by not making users source BIOS
> files. The HLE hands over to the cartridge at once, with no title screen.
>
> Evidence gathered the same day, before any HLE code: the corpus ships the real
> BIOS with a `[h1] (no title delay)` hack, which differs in exactly three bytes
> (`$13F1`-`$13F3`, a `CALL $1968` into the delay loop, NOPped out). The smoke
> run on it, `ledger/smoke-real-bios-nodelay.tsv`, against the normal BIOS: no
> title lost a verdict, one gained (Tournament Tennis, STATIC to ALIVE), and
> hand-over moved from about frame 669 to frames 13-26. Limit: the script's
> presses were timed for the delay, so this cannot see a game that misbehaves
> when pressed in its first seconds.

**What "correct" is measured against for the VDP and PSG.** Test ROMs exist for
the TMS9918A family. Finding out which ones, and whether they print their own
verdicts the way the suites RustStation uses do, shapes the whole harness list.
