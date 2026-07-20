# LoopTic internals

This document is the contributor-oriented overview of how LoopTic is
implemented. It connects the source layout, Embassy tasks, clock domains,
musical state, UI, audio renderer, and persistent storage.

For narrower contracts, see the [sampler architecture](sampler-architecture.md)
for real-time audio details and the [song storage architecture](song-storage.md)
for flash geometry and recovery. User-visible behavior belongs in the
[user guide](user-guide.md).

## Design constraints

LoopTic is built around five constraints:

1. Audio must deliver one 22,050 Hz frame continuously, even while the UI is
   busy.
2. The RP2040 has no heap-backed operating system; all long-lived state and
   workspaces must have fixed bounds.
3. Samples are read directly from execute-in-place flash, while flash erase or
   programming temporarily makes that same bus unavailable.
4. Musical edits, Track painting, and song loading must become visible
   atomically at audio-block boundaries.
5. A firmware update must not overlap the final 2 MiB flash partition that
   contains songs.

The resulting design separates a HAL-free, host-tested core from a small
RP2040 integration layer. The audio task owns its real-time objects and takes
bounded control snapshots; slower tasks never run inside the per-frame path.

## Repository map

| Path | Responsibility |
| --- | --- |
| `src/lib.rs` | `no_std`, HAL-free domain core: WAV parsing, Patterns, timing, transport, Tracks, sampler, shared state, song codecs, selection, UI controller, LED helpers, and most unit tests. |
| `src/main.rs` | Embassy/RP2040 integration: boot, peripherals, tasks, physical input, audio service, storage orchestration, and OLED/NeoPixel drawing. |
| `src/load_control.rs` | Measured audio-load policy, thresholds, quality shedding, and staged recovery. |
| `src/sample_assets.rs` | Stable names, paths, and `include_bytes!` catalog for the 24 embedded WAV files. |
| `src/flash_storage.rs` | Flash geometry, superblock classification, initialization, and the ARM-only `sequential-storage` backend. |
| `tests/storage_backend.rs` | Host integration tests for full geometry, churn, reopen, and injected power cuts. |
| `memory.x` | Authoritative 6 MiB firmware / 2 MiB storage memory map. |
| `build.rs` | Parses and asserts `memory.x`, generates matching flash-driver constants, and installs linker scripts. |
| `.cargo/config.toml` | Default embedded target plus firmware, flash, host-test, and size aliases. |
| `scripts/check-firmware-layout.sh` | Verifies ELF load ranges and every UF2 block stay below song storage. |
| `samples/` | Embedded audio and its provenance documentation. |

`src/lib.rs` is intentionally host-testable but has grown into a large module.
When adding a new subsystem, preserve its HAL independence and consider moving
it into a focused core module rather than extending the monolith further.

## Hardware and peripheral allocation

The supported target is the Adafruit MacroPad RP2040 with 8 MiB external flash
and a passive MacroPadSynthPlug PWM filter.

| Function | MacroPad pin | RP2040 resource |
| --- | --- | --- |
| SynthPlug audio | GP20 / STEMMA SDA | PIO0 SM0, DMA CH0 |
| 12 NeoPixels | GP19 | PIO1 SM0, DMA CH1 |
| Rotary encoder | GP17, GP18 | PIO1 SM1 |
| Encoder push | GP0, active low | Debounced GPIO |
| Voice keys 1–9 | GP1–GP9, active low | GPIO with pull-ups |
| Mute, Volume, Return | GP10–GP12, active low | GPIO with pull-ups |
| OLED data | GP26 SCK, GP27 MOSI | SPI1 at 10 MHz |
| OLED control | GP22 CS, GP23 reset, GP24 D/C | GPIO outputs |
| Built-in speaker enable | GP14 | Held low |
| Fault/underrun status | GP13 | Latched GPIO output |
| SynthPlug MIDI input | GP21 / STEMMA SCL | Reserved, not configured |

The SynthPlug duplicates the filtered PWM signal to both jack channels. It is
a line output, not a speaker driver.

## Boot and task topology

Boot initializes the shared states, holds the internal speaker off, parses and
validates every embedded WAV, configures the audio PIO machine, and then starts
two Embassy executors. Catalog validation failure stops boot with silent audio
and latches GP13. OLED initialization failure parks only the display task while
audio continues, and also causes GP13 to latch.

```text
                         interrupt executor
 SharedState snapshots  --------------------> audio_task
       ^                                      Sequencer + load policy
       |                                      DMA CH0 -> PIO0 -> GP20
       |
       +---------------- critical-section mutex ----------------+
       |                                                        |
 controls_task --------> UiController/actions          storage_task <----> flash
 GPIO + encoder              |                              |
       |                     v                              |
       +------------------ UiState -------------------------+
                              |
                         display_task
                         led_task
                         thread executor
```

The interrupt executor runs `audio_task` from a software interrupt at priority
P2. The thread executor runs storage, controls, LEDs, and OLED work. The
storage task classifies the superblock and builds slot occupancy before audio
startup is released; a supported journal may repair an interrupted operation
during that scan.

| Task | Owned resources | Scheduling model |
| --- | --- | --- |
| `audio_task` | `Sequencer`, `AudioLoadController`, PIO0 SM0, DMA CH0, two 128-word buffers | One service per 128-frame DMA block; highest-level real-time work. |
| `controls_task` | Key GPIOs, encoder PIO, encoder button, debouncers, Mute gesture, acceleration | 1 ms key scan plus encoder events. |
| `storage_task` | Flash backend, 4,096-byte work buffer, 4,085-byte record buffer | Boot probe/scan and queued explicit operations; 5 ms idle polling. |
| `led_task` | PIO1 SM0, DMA CH1, 12 NeoPixels, GP13 | Refresh from current state every 5 ms. |
| `display_task` | SPI1 and SH1106 OLED | Checks a display-state key every 34 ms; flushes only on change. |

LED and OLED tasks schedule their next refresh from the current time. They do
not replay missed periods after audio pressure, which prevents a visual
catch-up queue from becoming UI latency.

## Shared state and publication

There are two critical-section mutexes:

- `SharedState` contains persistent musical controls plus transport mailboxes,
  latest trigger timestamps, load state, and diagnostics.
- firmware-only `UiState` contains `UiController`, the slot occupancy/current
  song view, dirty baseline, pending storage operation, and short display
  feedback.

The audio task exclusively owns `Sequencer`. At block boundaries it consumes
transport and preview mailboxes, snapshots timing/gain/mute state, and copies
Pattern or Track data only when their revisions changed. It never locks shared
state for each rendered sample.

Three revisions have different purposes:

- `song_revision` advances once for each logical persistent edit and is
  compared with the clean Save/Load revision for the root `*` marker.
- `pattern_revision` plus a pad dirty mask limits Pattern copies into the
  sequencer.
- `track_revision` protects Track publication and lets a precomputed paint be
  committed only against the source timeline it copied.

Batch edits synchronize a whole selected mask under one lock and increment the
song revision once. Track painting constructs a complete candidate outside the
mutex, then uses the Track revision to commit all-or-nothing.

The Tracks display and cursor helpers deliberately copy fixed state under a
critical section before doing projection work outside it. This avoids lengthy
computation while interrupts are masked, but the copies themselves—roughly a
full `SharedState` for projection and a 1,540-byte timeline for painting—are
important SRAM and interrupt-latency regression watchpoints.

Compile-time size ceilings make growth of `SharedState`, `UiState`, the Track
timeline, and task-specific snapshots fail visibly. The LED task captures only
the selection and display fields it consumes. Track publication remains a
bounded copy under the critical-section mutex: a lock-free double buffer would
require unsafe shared-memory synchronization between the thread and interrupt
executors, which is not justified without hardware evidence that the bounded
copy misses its deadline.

## Clock domains

Three clocks must not be confused:

1. The hardware/DMA frame epoch is monotonic while audio runs. It timestamps
   visual pulses and service diagnostics.
2. `song_position_frame` is the finite next-unrendered frame within the current
   song. Pause freezes it; Loop and seek can move it backward.
3. Each voice has a rational Pattern clock based on its Beats and effective
   Cycle length.

The UI's Tracks timecode reports the second clock. Reset and Load restart it at
zero but do not clear the first clock or accumulated diagnostics.

## Musical timing and Patterns

Each voice has a Beats count, an optional Cycle-length override, a Pattern
Cycles multiplier, a sample ID, a 256-bit enable map, and 256 trigger levels.
An absent Cycle override follows the global Cycle length.

For `b > 0`, ticks are placed on the frame-zero global grid at rational
fractions of the effective Cycle. Deadlines use wide integer arithmetic and
carried remainders so non-integer samples-per-tick do not accumulate drift.
Contiguous rendering then advances mostly by addition; a non-contiguous
recovery computes overdue ordinals and coalesces safely.

Pattern step is derived from the one-based tick ordinal modulo
`Beats × Pattern Cycles`. Changing Beats or effective Cycle length realigns a
voice to the global grid at the next block boundary. Changing only Pattern
Cycles keeps the pending timing deadline and derives the new step from the
same ordinal. Hidden enable bits and trigger levels never move or rescale.

Multiple due ticks from one voice on the same output frame coalesce to one
start using the loudest due trigger level. Different voices never coalesce,
even if they use the same sample.

## Finite transport and Tracks

The sequencer stores song length in frames, a runtime transport state, End
behavior, current position, live-audition mask, and a canonical Track timeline.
Position always names the next frame to render.

Pause fades active voices and freezes all song/Pattern phases. Resume at the
unchanged paused position preserves phase. An explicit play-from seek is
inclusive, so a hit exactly at the cursor can start. At song end:

- Loop fades active voices, seeks every Pattern clock to frame zero, and
  continues immediately.
- Stop fades active voices and holds the exclusive end position until the next
  Play restarts from zero.

`TrackTimeline` stores at most 256 absolute changes in parallel fixed arrays:

- `frames[i]` is an absolute song frame;
- `gate_masks[i]` is the resulting nine-bit enabled mask from that frame; and
- no point means all voices enabled.

Frames strictly increase, masks contain only nine bits, and adjacent equal
masks are removed. A point applies before a scheduled hit at the same frame.
Painting builds a candidate timeline and therefore cannot partially alter the
song if canonicalization exceeds capacity.

Track gates participate only in start admission. Pattern clocks and visual
events advance through disabled spans, and existing tails continue. Live
audition replaces the gate/mute decision for held voices but still uses their
scheduled Pattern and trigger level; it does not create an immediate event.
Conceptually, a due enabled Pattern hit reaches allocation when:

```text
live_audition || (track_enabled && !ordinary_mute)
```

A zero trigger level remains visual-only and skips allocation even during
audition.

Projection is derived from the current timing and Pattern against absolute song
frames; stored Track boundaries never move when timing changes. Rasterization
sweeps canonical gates and per-voice Pattern ranges into 37 OLED rows. Dense
hits coalesce by row, with an enabled event taking priority, and active masks
produce the vertical span lines.

## Audio pipeline

All 24 WAV assets are embedded as immutable bytes in XIP flash. Boot validates
RIFF structure and requires mono signed 16-bit PCM at 22,050 Hz. The real-time
path holds lightweight views; there is no filesystem, decoder, streaming
cache, PCM copy, or heap allocation.

For each 128-frame block the sequencer:

1. applies current render policy and block-boundary control updates;
2. advances finite transport and per-voice clocks;
3. resolves Pattern enable/level, Tracks, Mute, and audition;
4. allocates due scheduled voices and at most one latest-wins preview;
5. mixes active voices and fade tails into a bounded signed 32-bit sum;
6. applies live pad-gain ramps and one master-gain ramp;
7. saturates to signed 16-bit PCM; and
8. converts PCM into PIO PWM commands with carried-error dither.

DMA plays one 128-word buffer while the audio task renders the other. At
22,050 Hz a block lasts about 5,805 microseconds. The joined PIO FIFO holds
eight more frames, about 363 microseconds, as a safety reserve rather than
additional render budget.

### Voices, gains, and fades

The fixed pool has 24 primary voices and nine temporary steal tails. There are
no per-pad reservations or choke groups, so repeated one-shots overlap.
Scheduled allocation prefers a free slot, then the oldest same-pad voice, then
an inaudible victim, then the oldest global voice. Preview has lower priority.

When policy permits, stealing copies the outgoing voice to a 32-frame tail.
Mute, transport, Load, and Reset release active primary voices in place instead
of stopping abruptly. Pad and master targets use independent 64-frame ramps.
The effective gain order is captured Pattern trigger level × live pad gain ×
live master gain.

### Adaptive load control

`AudioLoadController` observes complete service time, an EWMA, a 64-block
window maximum, DMA launch cadence, FIFO handoff delay, voice peak, and
underrun count. The resulting policy applies to the next block.

Pressure first removes optional work: preview, full-cost dither, new steal
tails, and the normal eight-start-per-pad quota. Higher pressure lowers the
effective primary limit and releases excess voices; Emergency may trim work
immediately to protect DMA. Recovery is deliberately slow and staged to avoid
quality chatter.

These mechanisms shed audible quality, not time. Clock ordinals, Pattern
position, finite song position, and visual reports continue. GP13 latches only
for a real PIO underrun or fatal initialization/display fault, not ordinary
Pressure. Exact thresholds and recovery stages are specified in the
[sampler architecture](sampler-architecture.md#adaptive-load-control).

## Input, UI, OLED, and LEDs

`UiController` is a pure state machine in the core library. It owns navigation,
ordered selection, Pattern cursors, group-warning state, Tracks gestures, and
Songs flow, then emits bounded `UiAction` values. `controls_task` supplies live
musical values, applies actions to `SharedState`, and queues storage commands.
This keeps UI behavior host-testable without RP2040 peripherals.

Twelve active-low keys are sampled every millisecond and require five
consecutive samples to change debounced state. Return's raw physical level also
blocks encoder detents during its debounce window. After navigation, held
controls are suppressed until release.

The encoder is decoded by PIO. `UiEncoderAcceleration` keys acceleration by
target, direction, and elapsed time, preventing a quick context switch from
carrying a large step into another parameter. Screens that promise exact
one-detent movement bypass it.

OLED drawing consumes `UiDisplayModel`, not arbitrary controller internals. A
small display key includes the model, relevant displayed value, and Pattern
revision; unchanged frames are not flushed. Track raster calculation uses a
snapshot outside the display mutex.

The LED task composes selected white, palette trigger/preview color, Light-mode
preview, selection dimming, and global brightness. Trigger timestamps use the
monotonic hardware frame. Tracks hides selection and assigns blue Play/Pause to
the Mute LED.

## Persistence

Songs use two independent compatibility layers:

1. a CRC-protected superblock identifies the physical journal layout and
   pinned `sequential-storage` format; and
2. each map value has an `LTSG` envelope containing a little-endian `u16`
   record version and payload length followed by Postcard data.

Current song format v4 stores Song length and Tracks in addition to the v3
per-voice controls. V2 and v3 decode into v4 in memory with a three-minute,
fully enabled arrangement; v2 also receives no per-voice Cycle overrides. V1
and unknown versions are rejected as unsupported.

Track changes pack a 27-bit absolute frame and nine-bit gate mask into five
little-endian bytes. Decode rejects the unused upper nibble, invalid masks,
unordered or redundant points, bad ranges, invalid sample IDs/volumes, trailing
bytes, and truncated payloads before applying anything. A default v4 record is
2,661 bytes; the worst canonical 256-point record is 3,999 bytes, below the
4,085-byte per-item buffer limit.

| Persisted in `StoredSongV4` | Runtime only |
| --- | --- |
| Song length and Track timeline | Transport position/state, End behavior, cursor, zoom, audition |
| Global Cycle and per-voice overrides | Hardware frame epoch and active voices |
| Beats and Pattern Cycles | Selection order, UI cursors, warnings, gestures |
| Samples, all Pattern bits, all trigger levels | Preview mailbox and visual pulses |
| Latched global/per-voice Mute | Momentary Mute |
| Master/per-voice Volume | Brightness, load state, diagnostics, dirty metadata |

### Flash partition and journal

`memory.x` assigns boot2 plus firmware to `0x10000000..0x10600000` and songs to
`0x10600000..0x10800000`. The first 4 KiB song sector is the superblock; the
remaining 511 sectors form a `sequential-storage 8.0.0` map with `u8` keys for
slots 001–256. Layout version 1 is tied to that exact on-flash format.

Blank storage is not initialized at boot. On first explicit Save, firmware
checks whether every map sector is already erased, invalidates metadata, erases
nonblank map data if required, writes and verifies a descriptor, then commits a
one-bit marker. Unsupported valid layouts are not opened and may be reformatted
only after explicit confirmation. Corrupt metadata is never passed to the
journal backend.

RP2040 flash erase/program stalls XIP and cannot safely overlap the audio path.
Before every storage operation, the audio task fades voices at a block boundary,
drains and stops PIO on centered silence, and acknowledges quiescence. Storage
then performs the operation and re-primes audio. Sector loops yield to the
thread executor, so input, Busy state, and formatting progress remain visible
while musical time is paused.

Journal mutation and recovery rules, including raw Copy behavior and injected
power-cut expectations, are detailed in
[song storage architecture](song-storage.md).

## Build, linker, and memory model

The release profile uses optimization level 3, fat LTO, and one code-generation
unit. Hot render/allocation entry points are linked into SRAM; the much larger
PCM bank remains in XIP flash.

`memory.x` is the single address source. `build.rs` parses its regions, asserts
the exact 8 MiB geometry, emits constants consumed by `flash_storage`, and
installs the embedded linker scripts. `scripts/check-firmware-layout.sh`
independently checks program headers and UF2 targets. This makes an accidental
firmware/storage overlap fail at build verification rather than on a device.

There is no runtime heap. State uses fixed arrays, fixed Embassy task futures,
static DMA/storage buffers, and `heapless::String` for OLED text. Shared and
audio state intentionally duplicate Pattern/trigger maps and Track timelines so
the per-frame renderer never locks shared state.

The current measured flash, SRAM, stack-audit, voice, and deadline baselines are
maintained in the [resource budget](sampler-architecture.md#resource-budget).
ELF and UF2 file sizes are not flash-usage values because they also contain
debug or container data.

## Verification strategy

The core test suite runs on the host and covers:

- WAV validation and catalog bounds;
- rational deadlines, exact-frame coalescing, wrap, recovery, and timing edits;
- Pattern enable/level persistence and Pattern Cycles;
- finite transport, Track gating, projection/rasterization, painting, capacity,
  and audition;
- allocation, stealing, fades, gain ramps, dither, clipping, and load policy;
- ordered selection, group synchronization, every UI route, Return priority,
  Tracks gestures, and LED helpers;
- v2/v3 migration, v4 packing, malformed records, and atomic apply; and
- flash superblock classification.

The storage integration suite uses the actual 511-sector logical geometry. It
fills all 256 slots with maximum-size values, churns and reopens the map, and
injects a power cut at each journal mutation step for first write, overwrite,
Copy, and Delete. Recovery may expose the complete old or complete new value,
never a mixture.

Typical checks are:

```console
cargo fmt --check
cargo host-test --locked
cargo clippy \
  --target x86_64-unknown-linux-gnu \
  --no-default-features \
  --all-targets \
  --locked -- -D warnings
cargo clippy \
  --release \
  --target thumbv6m-none-eabi \
  --bin looptic \
  --locked -- -D warnings
cargo firmware --locked
elf2uf2-rs convert \
  target/thumbv6m-none-eabi/release/looptic \
  looptic.uf2
./scripts/check-firmware-layout.sh
```

Host tests cannot establish RP2040 interrupt latency, XIP-cache contention,
PWM audio quality, physical debounce ordering, OLED readability, analog flash
power-cut behavior, or sustained thermal/power behavior. The physical-device
acceptance workload remains the authority for those properties; see the
[hardware benchmark protocol](sampler-architecture.md#hardware-benchmark-protocol).

## Maintenance invariants

When changing the firmware, preserve these boundaries:

- Do not await while either shared-state mutex is held.
- Do not put OLED, NeoPixel, storage, or unbounded work in the audio service.
- Keep every real-time collection fixed-capacity and define its overflow
  behavior.
- Treat `song_position_frame` as the next frame to render and Track intervals
  as half-open.
- Apply same-frame Track changes before scheduled hits.
- Increment `song_revision` once per logical persistent edit and never for
  selection, transport, zoom, audition, or warnings.
- Validate a complete stored record before replacing live musical state.
- Increment song-record format for schema changes and physical layout version
  for any journal-format or geometry change.
- Keep `memory.x`, generated storage constants, the release ELF, and the UF2
  below the same `0x10600000` boundary.
- Re-run the hardware workload after changes to renderer cost, large shared
  copies, task priorities, XIP placement, or flash quiescing.

Return to the [README](../README.md) for setup or the
[user guide](user-guide.md) for device operation.
