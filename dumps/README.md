# dumps/

Cartridge images, save states and classifier output live here. **None of it is
tracked**, and the ignore rule is a blanket over the whole tree rather than an
extension list, so a copyrighted image cannot be committed by landing in a
subfolder somebody forgot to cover.

That choice is deliberate. An ignore rule elsewhere in this fleet was anchored
one level deep, because `*` does not cross a `/`, so anything in a subfolder was
never ignored while the comment above it claimed otherwise.

The owner has a ColecoVision collection at
`TrophyHubResources/emulator-resources/rom-collections/ColecoVision.7z`, which is
outside this repo and stays there.
