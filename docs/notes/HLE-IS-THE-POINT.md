# HLE is the point, and how it gets judged

Written 2026-09-27, before any emulation existed, when the repo was set up. It
records the objective and the questions left open at the start. Nothing here is
a measurement, because there was nothing yet to measure.

## The objective

Run the **majority of commercial titles with no BIOS dump present**. Requiring a
firmware file has always been a drag, and it is worst on machines like this one:
these are short games from 1982 that somebody wants to play on a phone in under
a minute, and the setup usually costs more than the session does.

The method is **trial and error against the corpus**. That is the right method
here. The BIOS interface is published, the corpus is finite, and the feedback
loop is a game either working or not. What it needs is a rule so it stays
evidence rather than opinion, which is the section below.

What travels from here to other machines is the HLE METHOD, not code: the
Odyssey2 and the Intellivision share no silicon with the ColecoVision (Z80
against Intel 8048 against CP1610, with three unrelated video and sound parts).

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
observed. A free-text compatibility column in another emulator of mine
accumulated 24 different spellings for the same fault before anyone noticed it
could not be grouped, which turned a work queue into prose. So the ledger gets a
fixed vocabulary from the start, and the verdict is derived rather than typed.

**Tests must be able to fail.** Prove a new test fails before trusting it. It
matters more than usual on a core whose method is trial and error, because "it
boots now" is exactly the kind of evidence that rots.

**"Majority of commercial titles" needs a denominator before it means
anything.** Decide what the corpus is and how many titles are in it early, and
state coverage as a fraction of that. The collection used is kept outside this
repo.

## Open questions at the start

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
those two facts pull in opposite directions.

> **DECIDED 2026-09-27: SKIPPED.** The emulation community prides itself on
> boot skip, it is the best user experience, and losing the legitimate boot is
> far outweighed by not making users source BIOS files. The HLE hands over to
> the cartridge at once, with no title screen.
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
verdicts the way the PlayStation CPU suites do, shapes the whole harness list.
