# Modloader phase 2: how the World engine opens an archive

Working notes for phase 2 (hooking the game's archive loader). Addresses are
for `game_scarlett_master.exe`, 120,181,760 bytes, image base `0x140000000`.
They move with each game build. This file holds only offsets and format
facts, no game data, as `docs/asset-policy.md` requires.

## The archive header (RPF9, 64 bytes, all 12 archives)

| Offset | Size | Seen | Meaning (best guess) |
|---|---|---|---|
| 0x00 | 4 | `9FPR` | magic, `0x52504639` |
| 0x04 | u32 | 0x360 (appdata0), 0x9580 (common) | size of header plus table plus trailer |
| 0x08 | u32 | 0x340, 0x8520 | size of header plus table; the trailer runs from here to 0x04 |
| 0x0C | u32 | 0x40, 0x3a10 | unknown |
| 0x10 | u64 | differs per archive | hash |
| 0x18 | u32 | `0x00047a00` everywhere | 0x1a (u16) feeds the table decode |
| 0x1C | u32 | `0x150` everywhere | read by the decrypt and parse steps |
| 0x20 | u16 | 3–7 | read by the parse step |
| 0x22 | u16 | 1 | read by the parse step |
| 0x24 | u32 | 2 (appdata0), 418 (common), 2078 (levels) | entry count |
| 0x28 | u32 | one of 4 values | key ID |
| 0x2C | u8 | 1 or 2 | read by the parse step |
| 0x2D | u8 | 0x0f | bit 0 read by the parse step |
| 0x2E | u8 | 0x09 | picks the decrypt mode (bit 0 set → mode 2) |
| 0x30 | u32 | 0x10 | read by the decrypt step |

The table after the header is encrypted (entropy 7.4–7.6 bits per byte). The
entry data that follows looks compressed or encrypted too. The game imports
only `BCryptOpenAlgorithmProvider`, `BCryptCloseAlgorithmProvider` and
`BCryptGenRandom`, so the decryption is the game's own code.

## Call chain

1. **`0x140fc21c0`** checks the magic of an archive already in memory and
   builds a 0x60-byte archive object (constructor `0x140fc2240`).
2. **`0x140fc2540`**, the archive object constructor for archives on disk:
   - `[rsi+8]` is the device that reads the file.
   - It calls `0x141fb2600` to read the header and table.
   - It calls `0x141fb2780` and then `0x1408ec200` to decrypt the table in
     place. A failure clears `[rsi+8]`, so the archive does not mount.
   - It calls `0x14087aec0` at **`0x140fc267b`** to parse the table into the
     object at `rsi+28`.
3. **`0x141fb2600`**, the header and table read:
   - It reads 16 bytes through the device's vtable `+0x60` (read at an
     offset) and checks the magic.
   - It allocates the size at header `+0x04` or `+0x08` and reads the rest.
   - When the two sizes differ, it also reads the trailer at `+0x08` into a
     second buffer.
   - The buffers end up in `[rsi+18]` (header and table) and `[rsi+20]`
     (trailer).
4. **`0x1408ec210`**, the decrypt chain. It returns false on failure.
   - `0x140a348a0`: if header `+0x2e` is set, it runs mode 1 or 2 over the
     table spans.
   - `0x1428ce530`: an optional step, when the device's flag bit 0 is clear.
   - `0x144e6f740` (through `0x141fb2300`): looks up a span in the table at
     `[rcx+10]`.
   - `0x1406bd050`, then `0x1408ec360`: the latter runs over the trailer
     region (header `+0x04` to `+0x08`).
   - Each step ends in **`0x1406bd160`**. It runs a cipher context over a list
     of (pointer, length) spans **in place**: `0x140613720` (update), then
     `0x140613740` (finish). It decrypts, and may also verify.
5. **`0x14087aec0`**, the table parse. It picks a layout from the state
   (0, 1 or 2) and header `+0x22`. It calls `0x14087af70` (which reads header
   fields `+0x1a`, `+0x1c`, `+0x20`, `+0x22`, `+0x24`, `+0x2c` and `+0x2d`)
   and then `0x14087af40`, `0x14087af50` or `0x14087b0b0`.

## Where the hook goes

When `0x14087aec0` returns at `0x140fc2680`, the table is decrypted in
`[rsi+18]` and parsed at `rsi+28`. Adding or replacing entries there needs no
keys, and it runs after any check on the file's bytes.

Mod entry data can't come from the archive file. The plan is to give mod
entries offsets past the end of the real archive, and have the runtime's file
layer (`ReadFile`, DirectStorage) serve reads at those offsets from the mod's
loose file. The game then reads mod data through its normal path. For that,
mod entries need the flags for stored data (not compressed, not encrypted),
which the table dump below should show.

## The decrypted table (from `SCARLETT_RPF_TABLES=1` dumps, 2026-11-28)

The probe confirmed the hook works: the tables come out decrypted (entropy 5–6 bits per byte
instead of 7.4–7.6). 1,566 archives were read at boot, the 12 top-level ones and nested
archives (entries that are archives themselves). The parser keeps pointers into the table
(state `+0x08` onward):

| State | Points to | appdata0 | common |
|---|---|---|---|
| `+0x08` | the table itself (offset 0) | 0 | 0 |
| `+0x10` | section A | 0x40 | 0x40 |
| `+0x18` | section B: digests and a 256-byte signature | 0x60 | 0x60 |
| `+0x20` | the column directory | 0x1b0 | 0x1b0 |
| `+0x28` | the entry-to-row map | 0x240 | 0x240 |
| `+0x30` | the column data | 0x250 | 0x1298 |

**The column directory** has 24 bytes per column: a u64 tag, a u32 row count, a u32 offset
(from the column data, or from the trailer for trailer columns), a u16 row size, a u16 flags
field (1 means the column is in the trailer), and a u32 pad. Header `+0x20` is the number of
columns and `+0x22` the number of trailer columns. Tags seen:

| Tag | Row size | Rows | What it is |
|---|---|---|---|
| `33f70dfaf2c50989` | 0x18 | every entry | where the data is and how big (below) |
| `73bcb72dce74f11f` | 8 | every entry | name hash (`0x145520ac0` reads it) |
| `e0c12d7a5d4884c5` | 8 | compressed entries | compression parameters; the read path asks for it |
| `b7076b6245d35fb5` | 4 | some entries | goes with the compression info (`0x145305ee0`) |
| `286030a358a789fb` | 0x40 | some entries | holds 16-byte digests, probably encryption or chunk data |
| `b6f5bdb2c66da2e9` | 8 | every entry, in the trailer | per-entry hash; whether it is checked on read is unknown |

**The entry-to-row map** holds, for each entry and each non-trailer column, one index (header
`+0x2c` gives 1 or 2 bytes per index). The index is that entry's row in the column, or all ones
when the entry doesn't have it (`0x144db65b0`). Entries are in name-hash order, which is not
the order of the rows.

**The main record** (`33f70d…`, 24 bytes):
- `+0x00`: 40-bit offset in 16-byte units; the data is at `offset * 16` in the archive file.
- `+0x05`: u8, the entry's kind. `0xff` is an ordinary file, 0 an archive inside the archive, and
  other values (`0x0b`, `0x40`, `0x5d`, `0x68`, …) resources of that type. Setting it to 0 for an
  ordinary file makes the game mount it as an archive, which fails and stops the game.
- `+0x08`: u64 stored size in 16-byte units.
- `+0x10`: u64 size once unpacked.

Whether an entry is compressed is said by its columns, not by `+0x05`. Ordinary files with no
compression row are stored as they are, the stored size covering the unpacked size (7,162 of
7,162 checked in nested archives); with a compression row (`…01000803`, the u16 at `+4` looks like
a count of 64 KiB chunks) the data is zlib, or stored as it is when the stored size covers the
unpacked size. Every ordinary file and resource has a `b7076b…` row; it is not a CRC32, Adler-32
or MurmurHash3 of the contents. Archives inside archives (kind 0) have no other rows and start
with `9FPR`.

**Names.** An entry's name hash is MurmurHash3 x64_128 with seed 0, keeping the low 64 bits.
The input is the path relative to the archive's mount root (`common:/`, `platform:/`), with
`A`–`Z` lower-cased and `\` turned into `/` (the table at `0x1465b23b0`). Paths from the exe
match real entries this way, for example `data/version.txt` in `common.rpf`. The lookup is
`0x145305880`: it hashes the mount prefix and compares it with the archive's (`+0x70`/`+0x78`),
then calls `0x14056f130`, a binary search on the name hash that also compares an extension type
ID from `0x14056ee50`.

## Next steps

1. **Hook primitive.** Patch a `call rel32` site to call a stub allocated
   within ±2 GB of the image. The stub jumps to runtime code, which calls the
   original function. It refuses to patch when the bytes at the site differ
   from what was expected, so a different build is left alone.
2. **Table probe** (diagnostic, off by default): at `0x140fc267b`, log each
   archive mounted and dump its decrypted table to `<data_dir>/rpf-tables/`,
   outside the repo. That shows the entry layout: name hashes or strings,
   offsets, sizes and flags.
3. ~~Work out the entry layout from the dumps.~~ Done (above).
4. **Replace an entry.** Built (`scarlett-hle/src/modules/archives.rs`, `replace`). After the
   title's parse, an entry whose name hash (with or without the extension) matches a mod file gets
   an offset past the end of the archive (each file 4096-aligned), both sizes from the mod file,
   and no rows in the compression and digest columns; its kind (`+0x05`) is kept. Resource entries
   are left alone for now. Bytes 6–7 of the record and
   the `b7076b…` column are left as they are.
5. **Serve the reads.** Built. `CreateFileW`, `CreateFileA` and `CreateFile2` note handles to
   archives that mods change; `ReadFile` past the real end, DirectStorage `read_at` past it, and
   `GetFileSizeEx` are answered from the mod files.
6. Test in game whether a replaced entry reads correctly (an unencrypted zlib entry first, then
   an encrypted one), and whether the per-entry hash in the trailer is checked on read. If it is,
   update it or skip the check.

Entries with a row in `286030a358a789fb` are encrypted: their bytes don't inflate as zlib
(`data/version.txt` in `common.rpf`, for example). Entries without one inflate as plain zlib
(`data/audio/speechevents.meta`).
