# bios/

**You should not need anything in here.** ColecoRust boots a cartridge on its
own HLE BIOS, with no firmware dump present. That is the point of the project
rather than a nice-to-have.

What this directory is for is DEVELOPMENT. A real 8 KB BIOS is the oracle: when
the HLE and the hardware disagree about what a game sees, the fastest way to
find out which is wrong is to run both and diff. Drop a dump here and the dev
harnesses will use it when asked.

Nothing here is tracked. The BIOS is copyrighted code and can never be
committed, in any revision.
