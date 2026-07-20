# Song storage architecture

LoopTic reserves the final 2 MiB of the MacroPad's 8 MiB W25Q64 flash for
explicitly saved songs. Addresses in the flash driver are device-relative;
addresses in the linker and debugger use the RP2040 XIP window.

| Region | Flash offset | XIP address | Size |
|---|---:|---:|---:|
| boot2 + firmware | `0x000000..0x600000` | `0x10000000..0x10600000` | 6 MiB |
| storage superblock | `0x600000..0x601000` | `0x10600000..0x10601000` | 4 KiB |
| wear-levelled song map | `0x601000..0x800000` | `0x10601000..0x10800000` | 511 sectors |

`memory.x` ends the linker-owned `FLASH` region at `0x10600000` and declares
the tail as a separate `SONG_STORAGE` region. Linker assertions require the
regions to be contiguous and to end at the physical 8 MiB boundary. No program
section is assigned to `SONG_STORAGE`, so the generated ELF and UF2 contain no
song-storage blocks. The normal `cargo flash` BOOTSEL/UF2 workflow and normal
firmware-sector programming therefore preserve stored songs.

This partition is a strong firmware-linking guard, not protection against a
whole-chip erase. `probe-rs --chip-erase`, a debugger's mass-erase command, or
manually erasing the tail still destroys saved songs. Avoid chip erase when
updating a unit whose songs should be retained.

## Version gates

The first 256-byte program page of the superblock sector contains an immutable,
CRC-32-protected header. Boot inspects the complete 4 KiB metadata sector, so a
write in any byte reserved by the current format is corruption rather than a
retryable initialization. On the first explicit Save, initialization verifies
whether the complete 511-sector map is already erased, then invalidates the
metadata sector. A nonblank map is erased; an already-erased map is left alone
to avoid a long, unnecessary first-save stall. Initialization then writes the
first page with a one-bit commit marker erased and clears that bit
only after the complete descriptor verifies. The song map is never opened
before the committed image reads back correctly. The remaining bytes stay
erased. The header records:

- a distinct LoopTic storage magic and superblock-header version;
- the LoopTic physical/backend layout version;
- the pinned `sequential-storage` on-flash format family;
- the song record version used when the partition was initialized;
- the partition size and relative start of the song map.

Boot probes this raw sector before constructing `sequential-storage`. A fully
erased sector is `Blank`; an exact partial image of the current descriptor
interrupted before the one-bit commit is
`Incomplete`; a valid supported page is `Ready`; a CRC-valid but unknown layout
is `Unsupported`; and damaged committed metadata is `Corrupt`. Blank and
Incomplete storage can be initialized/retried only by the next explicit Save;
retry unconditionally repeats the complete map erase so stale or partially
erased map data can never be adopted by a fresh descriptor.
Unsupported and Corrupt stores are never opened, repaired, erased, or
reformatted automatically. A valid Unsupported header offers an explicit,
Cancel-first Format confirmation; accepting it destroys every stored song and
creates the current layout. Corrupt storage does not offer that path because
its identity and bounds cannot be trusted. This ordering is essential because
a storage library cannot be allowed to interpret and repair an on-flash format
it does not understand.

Each stored song also has its own magic and schema version. The superblock
protects backend compatibility, while the song-record decoder distinguishes a
supported song, an unsupported older/newer schema, and corrupt payload data.
That second version gate permits future firmware to migrate records one song at
a time without changing the physical partition.
Current saves use song format v3, which adds an optional Cycle-length override
for every pad. The decoder also accepts v2 and migrates it in memory by assigning
every pad to the global Cycle length, exactly preserving v2 timing. Loading does
not rewrite the stored bytes; the next Save after an edit or Save-as writes v3.
Raw Copy retains the source record's original version. V1 and unknown newer
records are reported as unsupported and are not rewritten automatically. This
record-schema change
does not alter the superblock or journal geometry, so the physical storage-layout
version remains 1.

## Map and operation rules

The remaining 511 erase sectors are a `sequential-storage` map with `u8` keys:
key 0 is display slot 001 and key 255 is display slot 256. Values are the raw,
self-versioned song records. The backend exposes a one-pass 256-bit occupancy
scan plus load, save, and delete operations. Animal names are firmware-owned
labels derived from the slot and are not stored in flash.

Two MiB is deliberate. A complete V3 song is about 2.7 KiB and therefore each
live value occupies most of one 4 KiB erase sector. A 1 MiB partition contains
exactly 256 sectors, but the journal requires a next-page migration buffer. It
therefore cannot operationally hold 256 one-sector live records and still
update or collect them. The 511-sector map holds all 256 live slots plus the
required buffer while retaining substantial append/compaction workspace and
distributing erases cyclically.

The frozen Postcard/Serde V3 DTO stores the global Cycle length, nine Beats
values, nine Pattern Cycles multipliers, nine optional per-pad Cycle-length
overrides, nine sample identifiers, nine 32-byte enable maps, nine sets of 256
trigger levels, global/per-pad latched mute, and master/per-pad volume. It is not
`SharedState`: playback position, voices, scheduler and UI cursors, previews,
momentary mute, brightness, dirty flags, adaptive-load state, and diagnostics
are intentionally excluded. Nested 32-byte chunks keep serialization bounded
and allocation-free. Decode validates every range and sample identifier before
atomically applying the complete value. The current encode buffer is 3,072
bytes; unknown versions and trailing, truncated, or semantically invalid data
are rejected without changing live state.

Flash erase/program code temporarily makes XIP unavailable and can exceed the
audio DMA/FIFO reserve. All write operations—including first initialization,
Save, Copy, and Delete—must therefore run only after the audio task has faded
voices, reached a block boundary, stopped the PIO stream on centered silence,
and acknowledged that flash is quiescent. Once a flash operation begins it is
not cancellable. Loading and the boot occupancy scan also happen while audio is
stopped; decoded musical state is applied atomically at a later block boundary.

Storage is initialized only by the first explicit Save. Booting blank storage
does not write it. Initialization first verifies whether the map is erased,
then erases the metadata sector. It erases the remaining 511 sectors only when
that verification found data or initialization is resuming from an incomplete
descriptor. Map scanning and erasing proceed one 4 KiB sector at a time and
yield to the executor between sectors, keeping Busy display and LED feedback
alive while audio remains paused. Explicit reformatting reports the completed
erase-sector percentage on the OLED. This still clears every potentially stale map byte; the
metadata-first write ordering makes a power cut during an erase reboot as
Blank so the full erase can be retried safely. Saving an unchanged record
silently skips the flash operation.

An ordinary occupancy scan only reads flash. If a supported journal contains
an interrupted operation, however, `sequential-storage` may automatically
repair it while scanning. The scan and any such recovery finish before audio
starts; unsupported or corrupt superblocks are never handed to the journal
library.

Root Save writes the current slot or redirects an unslotted song to Save-as.
Load validates a complete record before replacing musical state. Copy transfers
stored source bytes to a stored destination without loading them into the live
song. Delete removes the key while leaving live musical state untouched; if it
was the current slot, the live song becomes Unsaved. A Copy whose destination
is the current slot marks the live song dirty because the backing record no
longer matches it.

Host tests inject a power loss at every journal mutation point in the first
record write, overwrite, Copy, and Delete, then reopen the map as a reboot
would. Recovery exposes either a complete previous value or a complete new
value, never a mixture. Another test fills the actual 511-sector geometry with
all 256 near-maximum records, overwrites and deletes entries, and reopens it.
Synthetic metadata tests cover partial initialization and unknown layouts;
codec tests cover v2 migration, current/unknown record versions, truncation,
trailing data, and semantic corruption. End-to-end initialization cuts and abrupt cuts on
physical hardware remain acceptance tests because host fault injection cannot
model the W25Q64's analog behavior.

Layout version 1 is permanently bound to the exactly pinned
`sequential-storage 8.0.0` writer. Any dependency update that may change
on-flash bytes must increment LoopTic's storage-layout version, even if the
upstream crate keeps the same major version.
