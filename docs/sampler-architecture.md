# Sampler architecture

This document defines the real-time sampler behavior for LoopTic's 24-sample
bank. It is the implementation and benchmark contract behind the shorter
control description in the [README](../README.md).

## Audio dataflow and deadline

The RP2040 remains at the firmware's 125 MHz system clock. PIO0 consumes one
encoded PWM word for each 22,050 Hz audio frame while DMA alternates between
two 128-frame buffers. Each buffer lasts

```text
128 / 22,050 = 5.804988... ms
```

The complete active audio service therefore has a nominal 5,805 µs deadline.
The joined PIO TX FIFO holds eight additional words, or about 363 µs at this
sample rate. A launch-to-launch interval above 6,168 µs can exhaust both the
block and that FIFO elasticity. The FIFO is a final safety reserve, not extra
rendering budget.

```text
control snapshot             scheduled pad ticks       latest preview request
       |                              |                          |
       +------------------------------+--------------------------+
                                      v
                         exact-frame event resolution
                                      |
                         primary-voice allocation (24)
                                      |
                 +--------------------+--------------------+
                 |                                         |
        active primary voices                    forced-fade tails (9)
                 |                                         |
                 +---------------+-------------------------+
                                 v
           captured trigger gain x voice fade x live pad-gain ramp
                                 |
                            signed i32 sum
                                 |
                       independent master ramp
                                 |
                  i16 saturation -> PWM dither -> DMA
```

The audio task takes short snapshots of control state at block boundaries and
never awaits UI, OLED, or NeoPixel work. It renders the following buffer while
DMA plays the current one. A preview published before a snapshot starts at the
next 128-frame boundary; one published just after a snapshot starts at the
following boundary. The safe double-buffered path therefore has a worst-case
control-to-audio latency just under two blocks (about 11.61 ms).

Timing covers two nested scopes. `render` measures only PCM scheduling,
allocation, mixing, and PWM encoding. `service` starts before load-policy work
and includes the DMA launch, shared-state snapshots, pattern synchronization,
rendering, and report publication. The controller uses the completed service
measurement for the **next** block. Separate launch-cadence and post-DMA
handoff measurements expose delays that a render-only timer would miss.

## XIP PCM catalog

The catalog contains 24 mono, signed 16-bit, 22,050 Hz PCM WAV assets totaling
1,064,964 bytes. Assets are compiled into flash and exposed as validated
immutable `WavPcm16` views. Sample data stays in execute-in-place (XIP) flash:
there is no filesystem, boot-time PCM copy, heap allocation, streaming cache,
or decoder in the real-time path.

Catalog order is explicit and stable. Each entry supplies a compact sample ID,
a short OLED name, and its PCM view. A pad stores only the selected ID. A voice
captures that ID when it starts, so later browsing changes future triggers but
does not redirect an existing tail.

Every asset is validated before audio starts. A malformed, truncated, stereo,
compressed, wrong-rate, or non-16-bit asset is a firmware initialization fault:
audio remains silent and the GP13 fault indicator latches. IMA ADPCM and other
compressed representations are intentionally deferred; direct PCM gives each
voice constant-time, independently addressable XIP reads.

## Event and voice semantics

### Control state and modes

The OLED root menu is ordered `Beats`, `Song settings`, `Pattern`, `Tracks`,
`Sample`, `Light`, `Save`, `Songs`, and `Reset all`, with Beats highlighted at boot.
Encoder navigation moves one item per detent without acceleration and clamps at
both ends; the five-row window scrolls and its cursor survives mode entry and
Return. Selectable lists render their active row as a full-width white band with
black text, while continuation triangles remain contrasting at the left edge.
Encoder push enters the highlighted mode or invokes root Save; modes do not
close merely because a beat key is released.

Beat-key press edges update a persistent ordered selection; release edges have
no effect. Selection begins in single-voice mode, where a lone press uses the
exclusive toggle. Any chord of two or more debounced pads, at the root or in a
menu, replaces selection with exactly that chord in ascending pad order and
enters multi-select mode. While at least two members remain, lone presses
independently add or remove pads, new members append chronologically, and
removing the primary promotes the next-oldest member. Removing down to one exits
multi-select mode; subsequent lone presses are exclusive again. A later chord
always replaces the complete group.

`VoiceSelection` stores the nine-bit mask and fixed-capacity chronological
order. Its selected count determines whether multi-select mode is active, and
it exposes a validated group summary
containing the mask and primary pad. A multi-voice OLED header renders as
`P<primary>+<additional>`, while all selected LEDs receive equal selected-white
treatment. Pattern requires exactly one voice:
attempting to enter it with a group leaves the controller at the root and shows
`Pattern needs 1 voice` / `Deselect extras`. A chord pressed while Pattern is
open installs the new ordered group, closes Pattern, and shows the same root
warning. Selection changes and key releases never publish sample previews.

The bottom-right control is Return. It is white. On the scan that recognizes
its press, Return takes priority over other key-press edges and an
encoder-button press. A simultaneous Mute release is cancelled without a latch
toggle. The physical Return level blocks encoder detents throughout the
five-sample debounce window, so they cannot edit a value or queue a preview just
before Return is recognized. Return first closes a Pattern Cycles editor, a
Song-settings editor, or Tracks' End Behavior overlay while remaining in that
mode. Otherwise, Return closes the current mode or confirmation and discards
any still-uncommitted choice or control gesture,
preserves the complete selection, and restores the root at its remembered
cursor. A Return press while already at the root instead clears selection. Controls
already physically held at that point are blocked until release, preventing an
input intended for the previous screen from leaking into the root.

Encoder acceleration applies only when consecutive detents within 40 ms address
the same target and move in the same direction. Changing target or direction
starts again with the slow step.

Beats, Cycle length, Sample, and ordinary pad Volume edits resolve a non-empty
selection to its group summary and display the primary member's stored value.
If all selected values match, a detent computes from that primary value and
atomically assigns the result to the full mask. If they differ, the first
detent performs no mutation and opens a sticky warning containing the primary
pad and value. Further detents are ignored; encoder push atomically copies the
primary value to the group, and the opening detent is discarded. Return cancels
only the warning before resuming normal navigation. Selection or context
changes, Load, and Reset also cancel it. Opening a warning clears encoder
acceleration state, and a Volume warning remains confirmable after Volume is
released.

Mode behavior is:

- **Pattern:** with no selection, show `Select voice`; group entry is blocked at
  the root. Exactly one selected pad exposes
  a persistent `Cycles` multiplier, its direct-slot pattern list, and its own
  cursor. Beats controls tick cadence while `beats × repeat` controls Pattern
  wrap length. Every trigger row shows
  enable state and stored trigger level; `All` shows the rounded whole-map
  average. Slow turns move one row and eligible accelerated turns move ten
  rows, clamping at `Cycles` and the final visible trigger rather than crossing an
  endpoint. The encoder push toggles a trigger row or opens/confirms the safe
  `All` choice.
- **Beats:** with no selection, show `Select voice` and ignore encoder turns. A
  selected group atomically edits its primary's 0–256 Beats value with
  1/10-step acceleration and mixed-value confirmation. Each affected pad clamps
  Pattern Cycles and its remembered cursor independently without erasing hidden
  Pattern state. Encoder push has no action outside a warning, and Return closes
  the mode.
- **Song settings:** choose `Song length` or `Cycle length`. Song length is a
  persistent 1–5,999 second value, edited one second per detent. In the Cycle
  editor, no selection targets the global length with 10/100 ms acceleration;
  a selected group atomically edits its primary's stored Cycle length with the
  same acceleration and mixed-value confirmation. Zero means follow Global;
  50 ms is the minimum independent length, and values 1 through 49 are skipped.
  Mixed detection compares stored values, so zero differs from an explicit
  override equal to Global. Push or Return closes an editor back to the menu.
- **Tracks:** hide but preserve ordinary voice selection and render nine
  vertically scrolling projection columns. Filled/hollow dots distinguish
  enabled/disabled gate spans, enabled spans have center lines, and dense rows
  coalesce with enabled priority. Stopped turns move one projected boundary;
  held voice chords paint half-open spans atomically on release. Encoder-held
  turns select one of 16 display-only zooms; an unturned click opens runtime
  Loop/Stop choice. Mute is brightness-scaled blue Play/Pause. While playing,
  ordinary turns edit master volume and held voices live-audition future
  scheduled hits through Track and mute gates without persistent mutation.
- **Sample:** with no selection, show `Select voice`. For a selected group, each
  encoder detent moves the primary value one entry through the 24-sample catalog without
  acceleration and clamps at the first or last sample. Group editing atomically
  assigns the primary's resulting value and uses mixed-value confirmation. Each
  edit or synchronization queues no more than one preview through the primary;
  holding the physical encoder button while turning suppresses that preview
  without suppressing movement.
- **Light:** edit brightness from 0 through 100% with the established 1% slow
  and 10% accelerated steps while supplying a steady full-palette base state.
- **Save:** save the current live song back to its associated slot, skipping
  the flash write and returning silently when its edit revision is unchanged.
  With no associated slot, enter the Save-as browser instead. Selecting any
  Save-as destination immediately starts the write, including replacement of
  an occupied slot; neither Save path uses a confirmation.
- **Songs:** open bounded Load, Save-as, Copy, and Delete flows over 256 stable
  numbered/animal-named slots. Slot browsing uses 1/10-step acceleration and
  clamps rather than wrapping. Selecting a Load slot starts immediately when
  live state is clean. Dirty live state and an occupied source instead open a
  Cancel-first warning explicitly stating that unsaved changes will be lost
  before Load can proceed. Empty Load slots proceed directly to empty feedback.
  Until the boot occupancy scan completes, dirty Loads conservatively take the
  confirmation path rather than treating the not-yet-known map as empty.
  Copy and Delete retain their confirmations, as does Format when incompatible
  storage offers reformatting. Copy operates stored-slot to stored-slot without
  replacing live state. Busy and formatting progress overlays remain
  authoritative during flash work, and errors remain dismissible afterward;
  successful Save, Save-as, and Load operations return without a completion
  dialog. Copy, Delete, and Format retain their completion screens.
- **Reset all:** open `Cancel`/`Reset` with `Cancel` initially selected. Each
  detent moves one choice without acceleration and clamps at the ends. Pressing
  the encoder on either choice exits the confirmation.

Outside Tracks, Mute captures the persistent selected group at its press edge,
or Global if the selection is empty, and retains that target for the complete
tap/hold gesture. A group tap atomically sets every member's local latch opposite
the primary member's latch. A group hold momentarily mutes every captured member
and restores their persistent latches on release. Return retains priority and
cancels an active gesture. The Mute LED follows Global when selection is empty;
otherwise it represents the primary local latch and active captured gesture.
Volume instead resolves its target dynamically while held. Ordinarily it edits
the selected group or the master when selection is empty, using the common
mixed-value warning. On a selected pad's Pattern page, it edits the highlighted
trigger level; highlighting `All` targets all stored trigger levels. It then returns
encoder control to the open mode on release unless a warning remains open for
push confirmation. Tracks replaces Mute with transport and routes unmodified
encoder turns to master volume while playing.

Beat LEDs are composed in two stages. The base color is off when idle, steady
white for every selected beat, a pad's normal palette color for a 100 ms trigger
or preview indication, or the normal palette while Light mode is open. On a
selected key, a trigger contributes 20% of its palette color to 80% white before
global brightness is applied, so continuous fast triggers still read as selected.

If a selection exists, an additional 20% multiplier is then applied to every
unselected beat LED. Consequently idle keys remain off; trigger, preview, and
Light states on other pads are dimmed by 80% rather than replaced with a steady
light. Every selected beat retains its computed white or palette color, and no
multiplier is applied when selection is empty. The bottom controls do not
participate: their identities remain Mute red, Volume yellow, and Return white,
except that Tracks renders Mute dim/bright blue for stopped/playing transport.

### Pattern representation

Each pad owns 256 fixed slots. Its 256-bit (32-byte) enable map is initially
filled, and a separate byte per slot stores an independent linear trigger level
from 0 through 100%, initially 100%. At a beat division of `n`, scheduled step
`s` reads enable bit and level `s` directly for `0 <= s < n`; there is no
proportional range mapping or interpolation. Reducing a division exposes a
shorter prefix without modifying the hidden suffix, and increasing it reveals
the previous enable states and levels.

Pattern mode presents a scrollable `Cycles` row, then `All`, before the visible
trigger rows. Pushing `Cycles` opens a one-step editor bounded by
`floor(256 / beats)`; push or Return closes it. Increasing Beats clamps Cycles
when necessary without erasing hidden slots.
Each trigger row reports `ON`/`off` and its stored percentage. The row cursor
clamps at `Cycles` and the final visible trigger. `All` reports the whole enable
map as `ON`, `off`, or `mix` and its rounded average trigger level, not merely
the currently visible prefix. Pressing it opens a three-choice confirmation
containing `Cancel`, `All`, and `None`; `Cancel` is selected initially. The
choice also clamps at `Cancel` and `None`; the encoder button confirms it and
returns to the pattern list. `All` fills all 256 enable bits and
`None` clears all 256 enable bits. Committing either choice also resets every
one of the 256 stored trigger levels to 100%, including hidden and disabled
slots. `Cancel` leaves both maps unchanged. Return also cancels the choice,
preserves selection, and returns to the root. At division zero no trigger rows
are visible, but `Cycles` and `All` remain available; Cycles is fixed at 1x and
`All` still operates on the complete
map. Normal division changes and individual trigger edits never alter hidden
slots.

Holding Volume changes Pattern navigation into level editing. A slow detent
adds or subtracts one percentage point from the highlighted trigger; repeated
same-target, same-direction detents within 40 ms use ten-point changes. On the
`All` row, the same signed delta is applied independently to all 256 levels,
including hidden and disabled slots. This is a relative edit, not assignment to
the displayed average, so accents remain distinct until individual slots clamp
at 0% or 100%. Whole-map edits update the cached sum used for the rounded
average and publish one pattern revision.

Confirming Reset all is an atomic UI-to-audio state reset: set the global Cycle
length to 1000 ms, clear all per-pad Cycle-length overrides, set all Beats values
to zero and Pattern Cycles to 1x, turn every pattern bit on, set every trigger
level to 100%, assign pads 0–5 to AKU Kick and pads 6–8 to AKU Open Hat, turn
global and per-pad mute off, and set master/per-pad volume to 100%; clear pending
preview and visual-pulse state. At the next audio-block
boundary, active primary voices enter the normal 32-frame release. Existing
forced-fade tails are neither restarted nor cleared and finish their bounded
fades. Live gain ramps are frozen for those 32 frames, preventing a voice that
was at zero volume from rising toward the restored 100% target during release.
Emergency trimming is deferred for the same bounded window so it cannot hard-cut
the reset fade. Brightness, playback position, adaptive-load state, and
diagnostic counters are preserved. Completion returns to the root with
`Reset all` highlighted, selection empty, and no current song association.
Cancel and Return change no musical settings or song association and preserve
selection when leaving this confirmation. A confirmed Reset still clears
selection; Return clears it only when pressed at the root.

Persistent storage is deliberately outside the real-time renderer. The final
2 MiB flash partition, versioned superblock, Postcard song schema,
sequential-storage journal, explicit operation semantics, and audio-quiesce
protocol are specified in [song storage architecture](song-storage.md).
Selection order, multi-select mode, group summaries, and mixed-value warnings
are runtime UI state and are not encoded in a song; multi-voice editing does not
change either the song schema or physical storage layout.

### Cycle timing

The global Cycle length is the default timing interval for every pad. Each pad
may instead persist an independent Cycle-length override. The editor represents
the absence of an override as zero, which makes the pad follow the current
global value; any setting at or above 50 ms is stored as an independent value
and is unaffected by later global edits. For either source, a pad's Beats value
distributes that many scheduled ticks across its effective Cycle length. The Pattern
`Cycles` multiplier is orthogonal: it extends only the number of pattern slots
visited before wrap and never changes tick cadence.

The audio task snapshots all nine effective Cycle lengths with Beats and Pattern
Cycles at each block boundary. A Beats or effective-length change realigns that
pad to the first tick after the current boundary on the frame-zero global grid;
unchanged pads retain their pending deadlines. This permits pads with independent
Cycle lengths while keeping each rational clock deterministic and globally
phase-referenced.

### Exact-frame coalescing

Only multiple scheduled ticks belonging to the **same pad** and landing on the
same output frame coalesce into one request. If enabled ticks with different
trigger levels coalesce, the request captures their maximum level. The bounded
non-contiguous recovery path applies the same maximum rule to overdue steps.

No other requests coalesce:

- Separate pads start separate voices, even when they select the same WAV and
  trigger on the same frame.
- A preview and a scheduled hit remain independent, including when they use the
  same pad, sample, and frame.
- Hits on later frames always start new voices while earlier tails continue.

Pattern position, clock phase, and visual scheduling continue independently of
voice allocation.

There is a second, deliberately musical bound after exact-frame coalescing:
normal mode admits at most eight scheduled voice starts for each pad in one
128-frame block (about 1,378 starts/s per pad). Eligible dense-grid starts are
spread across the block instead of all being admitted at its beginning. A
request beyond that quota increments `load_shed_trigger_count`; its clock
ordinal, pattern position, and visual trigger still advance. Under measured
load pressure the same quota contracts to one start per pad per block. Thus
the 24-slot pool remains polyphonic without allowing a dense grid to run as
many audible allocations as there are due ticks in one service cycle.

### Primary pool

The sampler owns 24 primary slots without per-pad partitioning. There is no
one-voice-per-pad limit, so repeated hits and long tails overlap naturally and
one pad may occupy every available slot. Each primary records its pad, captured
sample, captured trigger gain, cursor, start age, and gain/fade state. Preview
and scheduled starts share the same playback representation after allocation.

For a scheduled audible request, allocation is deterministic:

1. Use a free primary slot.
2. Otherwise steal the oldest primary owned by the requesting pad.
3. Otherwise steal the oldest primary whose master × pad target gain is zero.
4. Otherwise steal the globally oldest primary.

A scheduled trigger with a stored level of 0% still advances pattern phase and
publishes its visual tick, but it is discarded before allocation: its captured
gain could never become audible. This is distinct from a nonzero trigger whose
live pad or master target is zero. That request may use a free slot and advance
silently. If the pool is full, it may reuse only a victim whose **current
ramped effective gain** has reached zero; otherwise it is dropped instead of
stealing an audible voice. Conversely, an audible request may select a
target-zero victim that is still ramping down, because the forced tail below
preserves its remaining sound without a hard cut.

Preview is deliberately lower priority. It uses a free primary slot, or may
replace the oldest voice from the previewed pad; if neither is available, the
preview drops. It never steals another pad's audible voice. Sample-mode
assignment changes use a latest-wins mailbox: all detents update persistent
state, but only the newest pending preview at a block boundary is considered.
Holding the encoder button during a detent publishes no request and does not
cancel an unrelated request already waiting in that mailbox.

### Forced fades and stealing

At normal quality, stealing does not immediately reuse a victim's waveform
without preserving a short tail. The victim is copied to one of nine temporary
tail slots and fades to zero over 32 frames, approximately 1.45 ms, while the
released primary slot starts the new voice.

If all nine tail slots are occupied, replace the tail with the fewest fade
frames remaining. Ties select the oldest tail and then the lowest slot. This
condition increments `fade_tail_overflow`; it must remain observable because
hard-replacing a nearly completed tail can still produce a small discontinuity.
The incoming scheduled trigger is not dropped. A lower-priority preview may be
dropped under its allocation rule instead.

Under timing pressure, existing tails are initially allowed to finish but new
steals do not create tails. Primaries above the Pressure limit enter the normal
32-frame in-place release. Emergency mode may remove existing tails and trim
old primaries immediately. Those are intentional, counted de-click quality
losses: preserving the DMA deadline takes priority over an inaudible
transition. Normal tail preservation returns in a later recovery stage.

Mute suppresses new matching requests and applies the same 32-frame fade to
matching primaries **in place**. In-place muting does not consume the nine steal
tails. The scheduler, patterns, and playback frame continue, and unmuting waits
for the next scheduled enabled trigger.

There are no choke groups. In particular, selecting open and closed hi-hats
does not make one terminate the other. Ordinary pool exhaustion follows the
deterministic rules above; measured timing pressure additionally invokes the
adaptive policy described below.

## Gain, preview, and mixing

Each scheduled pattern hit captures its slot's linear 0–100% trigger gain when
the voice starts. Later edits affect only future starts; existing primaries and
their forced-fade copies retain the captured value. Master and per-pad gains
remain independent live linear percentages. Each stores a current and target
value and traverses a 64-frame linear ramp, approximately 2.90 ms, whenever its
target changes. Pad and master ramps run independently and multiply; they are
not collapsed into one effective target. A new change starts from the ramp's
current value.

Each rendered sample is processed in this order:

1. Read signed PCM from the voice's captured sample and advance its cursor.
2. Multiply the captured trigger gain by the owning pad's live 64-frame gain
   ramp, then apply that effective gain and any 32-frame voice fade.
3. Sum all 24 primaries and nine temporary tails in signed `i32`.
4. Apply the independent live 64-frame master ramp to the sum.
5. Saturate once to `i16`, then feed the existing stateful PWM dither encoder.

No compressor, limiter, normalization, or automatic gain compensation is
applied. Deliberately loud combinations may saturate; they must never wrap.

Automatic preview uses a full 100% trigger gain with the same PCM, mute,
pad-volume, master-volume, mixing, and allocation paths as scheduled audio. It
also pulses the preview request's target-pad NeoPixel. Preview publishes only
that visual pulse; it does not advance the sequencer clock or pattern or change
phase. A muted pad does not produce an audible preview.

### Real-time implementation choices

The release profile uses `opt-level = 3`. On ARM, `Sequencer::render` and the
deterministic voice allocator are linked into `.data.ram_func` and copied to
SRAM at startup. PCM remains in XIP flash, but fetching these hot paths from
SRAM prevents their instructions from competing with simultaneous voice sample
reads for the same XIP cache/port.

The mixer uses signed `i32` fixed-point arithmetic. Trigger percentages use a
small Q16 lookup table, and exact 0% and 100% gain paths avoid unnecessary
multiplication or division. Other per-voice gains, 32-frame fades, and 64-frame
ramps use bounded power-of-two operations. `i32` is also a proven-safe
accumulator for all 24 primaries plus nine tails, and the master gain is applied
only once after that sum. Inactive slots are rejected before sample lookup or
gain work, active counts are maintained incrementally, and allocation-state
arrays are constructed only on frames with a scheduled or preview request.

Normal contiguous scheduling uses a rational deadline accumulator: wide
division is confined to a timing change or non-contiguous recovery, while the
frame hot path advances with additions and comparisons. Pattern playback uses
direct enable and trigger-level lookups by scheduled step. These changes
preserve the original ceiling-based global clock grid while avoiding
proportional pattern mapping; they change scheduling cost rather than musical
phase.

Full PWM dither makes sixteen carried-error decisions for each sample. The
pressure fallback uses a 17-entry mask table to distribute the same number of
one-bit duty extensions. It preserves the base duty, extension count, and
carried error exactly, but its spectral placement is coarser and cheaper.

The LED and OLED tasks wait one refresh interval from the current instant after
each pass. They intentionally do not replay missed periodic ticks: trigger
state is latest-value data, so catch-up writes would only extend UI latency
after an audio-pressure episode.

## Adaptive load control

The controller reacts to measured active service time, not merely nominal
polyphony. This matters because XIP-cache behavior, steals, ramps, and control
publication all affect whether DMA launches on time. Its thresholds are:

| Observation | Next-block response |
| --- | --- |
| Service or 1/16 EWMA at least 3,775 µs | Enter Pressure and prevent further voice-pool growth |
| Individual service at least 4,350 µs | In Pressure, reduce the primary limit to at most that block's peak minus one and release the excess |
| Individual service at least 5,225 µs | Enter Emergency and reduce the primary limit to at most the peak minus two |
| DMA launch gap above 6,168 µs | Enter Emergency; the block plus FIFO elasticity was exceeded |
| Post-completion handoff above 363 µs | Enter Emergency; the FIFO safety reserve was consumed |
| Any new PIO underrun | Enter Emergency and retain the hardware failure count |

The policies deliberately remove optional or bounded work in this order:

| Level | Primary behavior | Steal tails | Starts/pad/block | Preview | Dither |
| --- | --- | --- | ---: | --- | --- |
| Normal | Up to 24 | Nine, preserved | 8 | On | Full |
| Pressure | Cap growth and release excess voices over 32 frames | Existing tails finish; no new steal tails | 1 | Off | Coarse |
| Emergency | Hard-trim to the effective limit | Remove all | 1 | Off | Coarse |
| RecoveryDither | Hold the recovered voice limit | Off | 1 | Off | Full |
| RecoveryTails | Hold the recovered voice limit | Nine, preserved | 1 | Off | Full |
| RecoveryStarts | Hold the recovered voice limit | Nine, preserved | 8 | Off | Full |

Pressure sheds excess primaries with the same 32-frame in-place release used
for mute rather than a hard cut, while existing steal tails expire naturally.
Emergency hard-trims primaries, preferring releasing voices and then the
oldest, because a controlled discontinuity is better than missing DMA service.
If service remains at or above 3,775 µs while Emergency is active, its voice
limit keeps contracting by one instead of plateauing just below the emergency
threshold. Mute releases remain in-place and continue to use their normal
32-frame fade.

Recovery requires both the EWMA below 3,190 µs and the maximum in the current
tumbling 64-block window below 3,775 µs. Emergency needs 512 consecutive
healthy observations before returning to Pressure. Pressure then adds one
primary slot after each 256 healthy blocks. Once all 24 slots are available,
three more 256-block stages restore full dither, steal tails, and the normal
start quota before Normal finally re-enables previews. This hysteresis favors
stable audio over rapid quality chatter.

The controller can respond only after it observes a completed block, so it
cannot prevent the first isolated spike. The normal eight-start admission cap,
optimized hot path, FIFO reserve, and early 3,775 µs threshold bound that
exposure. Scheduler phase, pattern position, playback-frame time, and visual
triggers advance under every policy; only audible work is shed.

## Diagnostics and failure modes

Real-time diagnostics are saturating counters or high-water marks so that
observability cannot introduce a second failure:

- Last/maximum renderer time, last/maximum complete service time, service and
  renderer deadline misses, maximum DMA launch cadence, and maximum handoff
  delay.
- Current load level, current/minimum effective primary limit, EWMA, current
  64-block-window maximum, transition count, and the voice/limit/level context
  of the worst service observation.
- Primary, fade-tail, and simultaneous total-voice high-water marks, plus
  scheduled starts and deterministic steal categories.
- Scheduled drops caused by a live pad or master target of zero. Pattern
  triggers with a captured 0% level are skipped before allocation and therefore
  do not consume a voice or enter this pool-exhaustion category.
- Previews dropped by mute or the lower-priority allocation rule. Superseded
  latest-wins requests are observable from the mailbox's replacement return
  value when instrumenting control behavior.
- Forced-tail high-water mark and `fade_tail_overflow`.
- Load-shed previews, scheduled starts, fade tails, and primaries, plus the
  number of frames encoded with coarse dither.
- Output frames that required `i16` saturation.

There is no OLED diagnostics screen in the menu UI. Counters remain in
`SharedState` and `SamplerDiagnostics`, but the current firmware does not
publish them over RTT. A debugger can inspect them directly, or an explicitly
instrumented benchmark build can export them without restoring an OLED page.

Only a PIO FIFO stall increments the underrun count and latches GP13. A service
or render that exceeds 5,805 µs increments its separate deadline-miss counter;
it is valuable diagnostic evidence but is not itself reported as a hardware
underrun. Catalog validation and OLED initialization/flush failures are fatal
and also latch GP13, while NeoPixel work remains outside the audio refill path.
`SampleId` construction is checked, so an invalid index cannot reach the
real-time catalog lookup. Counters saturate instead of wrapping.

Pool exhaustion is expected behavior, not an underrun: scheduled audible hits
use the steal policy, hits silenced by live pad/master gain may drop, and
preview may drop. A 0% Pattern trigger is already skipped before allocation.
Tail overflow is also recoverable but diagnostic because it can expose a click.
Output saturation and deliberate load shedding are recoverable quality events,
distinct from timing failure.

## Resource budget

| Resource | Budget or baseline |
| --- | ---: |
| RP2040 physical flash | 8 MiB |
| Linker-owned firmware partition | 6 MiB |
| Persistent song partition | 2 MiB (511 journal sectors after superblock) |
| Current release flash use | 1,298,836 bytes |
| Complete 24-sample bank | 1,064,964 bytes |
| Free flash in firmware partition | 4,992,620 bytes (about 4.76 MiB) |
| Current executable text | 209,664 bytes (about 204.8 KiB) |
| Current linked static RAM span | 52,720 bytes |
| RAM remaining before runtime stack | 217,616 bytes (about 212.5 KiB) |
| RP2040 SRAM | 264 KiB |
| Pattern state per pad, per copy | 32-byte enable map + 260-byte trigger-level map/cache |
| Primary voices | 24 physical slots; adaptive effective limit 1–24 |
| Temporary steal tails | up to 9 fixed slots; adaptive limit 0 or 9 |
| Nominal audio-service deadline | 5,805 µs |
| Joined-FIFO reserve | 363 µs / 8 frames |
| Normal-to-pressure threshold | 3,775 µs service/EWMA |
| Pressure voice-reduction threshold | 4,350 µs service |
| Emergency threshold | 5,225 µs service |

These values come from the current optimized release ELF's loadable sections:
flash is 256 bytes boot2 + 192 bytes vectors + 209,664 bytes text + 1,082,844
bytes read-only data + the 5,880-byte data load image. Static RAM is 5,880
bytes data + 45,816 bytes BSS + the 1,024-byte reserved uninitialized region,
ending at the 52,720-byte heap/stack boundary.
The data section includes hot renderer/allocator code copied to SRAM; PCM
consumes no SRAM beyond catalog references. Enable and trigger-level maps are
duplicated deliberately between shared UI state and the audio sequencer so the
renderer never locks shared state per frame. Across nine pads, those two copies
use 5,256 bytes. Tracks adds two 1,540-byte canonical timelines, one each in
shared and audio state. Storage includes fixed 4,096-byte work and 4,085-byte
record buffers, the 511-page and 256-key journal caches, and the static
storage-task future; there is still no heap. Voice pools and diagnostics also
remain fixed-size. Release assembly puts the largest audited synchronous
controls-to-paint stack chain at about 11.6 KiB, comfortably inside the 212.5
KiB remainder. That is a regression bound, not a measured runtime stack
high-water mark.
ELF and UF2 file lengths are not device-usage figures because they include
debug information or container formatting.

## Hardware benchmark protocol

Benchmark the release profile on a MacroPad RP2040 at the default 125 MHz, with
the actual PIO/DMA PWM path and SynthPlug connected. Inspect the built-in RP2040
timer observations and retained counters with a debugger, or prepare a
benchmark build with explicit RTT logging. Retain both renderer-only and
complete service maxima, DMA cadence, handoff delay, load transitions,
effective voice limit, and underruns.

The concrete pre-change regression baseline is important. With pads 2–9 at
division zero, pad 1 selecting AKU Kick, a 1,000 ms base, and division 28, the
UI developed seconds of latency and the sound became distorted while audio
continued. The AKU kick is 11,265 frames (about 0.5109 s), so 28 starts/s
create an average 14.3 overlapping voices and alternate around 14–15 live
voices. This isolates steady mixer/XIP cost from a high event-rate scheduler;
it is an observation of the earlier firmware, not a benchmark result for the
optimized/adaptive build.

Exercise these cases separately and confirm their diagnostic counters:

1. Reproduce that one-pad AKU-kick case, sweep through division 28 and beyond,
   and operate the encoder and keys while recording load level, renderer and
   service timing, DMA cadence, deadline misses, and underruns through the
   selected debugger or benchmark export.
2. Twenty-four overlapping long primaries reading widely separated parts of
   the XIP catalog.
3. Continuous scheduled pressure that forces steals and first keeps all nine
   fade tails active, then verifies that Pressure stops adding tails and
   Emergency removes them.
4. Global and per-pad mute while the primary pool is full.
5. Simultaneous independent master and pad gain ramps, plus 24 active voices
   using non-default captured trigger levels. Verify the trigger × pad × master
   order and that existing voices retain their captured trigger level.
6. Fast Sample-mode browsing combined with scheduled hits, verifying normal
   assignment previews, silent push-and-turn assignment changes, immediate
   preview resumption after releasing the encoder button, endpoint clamping
   without replaying an unchanged sample, and Pressure dropping previews before
   essential clock work. Changing beat selection alone must not preview.
7. All nine pads at dense rates while rotating the encoder and refreshing the
   OLED and NeoPixels; verify the eight-start normal quota and one-start
   pressure quota without clock or visual-phase drift.
8. A dedicated tail-overflow run that verifies the deterministic replacement
   rule and counter without misclassifying it as a DMA underrun.
9. A recovery run long enough to observe Emergency/Pressure, voice-limit
   increments, RecoveryDither, RecoveryTails, RecoveryStarts, and Normal in
   order without chattering.
10. At division 8, edit slots on both sides of the division-4 boundary, shrink
    to 4, and return to 8; hidden slots must retain their prior state. Verify
    that the `All` choice defaults to `Cancel`, that `All` fills and `None`
    clears the complete 256-bit enable map regardless of its prior state, and
    that both affect hidden slots as well as visible ones and reset all 256
    trigger levels to 100%. Verify that `Cancel` and Return preserve both maps.
    Set distinct trigger levels on visible, hidden, enabled, and disabled slots;
    hold Volume on individual rows and on `All`, verifying 1%/10% changes,
    whole-map relative deltas, independent clamping, rounded average feedback,
    and persistence across shrink/expand until a committed `All` or `None`
    deliberately resets them. Verify row clamping at `All` and the final
    trigger, choice clamping at `Cancel` and `None`, and operation at division
    zero. Use the encoder button to toggle rows and to open/confirm the
    whole-map choice. On the Pattern page, verify that Mute still performs its
    normal selected-pad tap/hold gesture. Clear selection while remaining in
    Pattern and verify that Mute targets Global.
11. Starting with Beats highlighted, navigate the eight root entries in
    `Beats`, `Cycle length`, `Pattern`, `Sample`, `Light`, `Save`, `Songs`,
    `Reset all` order; verify five-row scrolling and clamping at both ends.
    Verify every selectable list uses a full-width white active row with black
    text, including after Return restores a remembered cursor, and that both
    scroll triangles stay visible against the row beneath them.
    Verify lone presses use exclusive selection until a 2+ chord at the root or
    in any menu replaces selection with its exact ascending group. In multi
    mode, lone presses must append or remove members chronologically, primary
    removal must promote the next-oldest, removal down to one must restore
    exclusive mode, and a later chord must replace the group. Entering Pattern
    with a group must remain at the root with `Pattern needs 1 voice` /
    `Deselect extras`; a chord inside Pattern must preserve the new group while
    returning to the same warning. Verify `P<primary>+<additional>` headers,
    equal white treatment for selected LEDs, and per-pad Pattern cursors.
    In Beats, no selection must show `Select voice`; group edits must directly
    use 1/10-step editing and independently clamp each member's Pattern Cycles
    and cursor without erasing hidden data. In Cycle length, verify direct
    10/100 ms global and group editing. A selected pad at zero must follow later
    global edits; clockwise from zero must select 50 ms, counter-clockwise below
    50 ms must return to zero, and values 1 through 49 must remain unreachable.
    Stored zero must compare differently from an equal explicit override.
    Nonzero pad values must ignore later global edits and persist independently.
    For Beats, Cycle length, Sample, and pad Volume, matching groups must update
    atomically on the first detent. Mixed groups must warn without mutation,
    ignore further turns, synchronize to the primary on push without replaying
    the discarded detent, cancel only the warning on Return, cancel on selection
    or context changes/Load/Reset, and restart acceleration at the slow step.
    Each detent or synchronization must publish one musical-state revision.
    Sample group edits and confirmations must queue at most one primary preview;
    a Volume warning must remain push-confirmable after Volume release. For a
    mixed-mute group, a tap must set every captured latch opposite the primary
    latch in one revision; a hold must momentarily mute the captured group and
    restore each persistent latch on release. Selection changes during the
    gesture must not retarget it, and the Mute LED must represent the primary
    local latch plus the active group gesture.
    Selection changes must not preview. Return must take priority over
    simultaneous key-press and encoder-button press edges, cancel uncommitted
    choices, preserve selection
    when leaving modes and confirmations, clear selection when already at the
    root, and block already-held controls until release.
12. Exercise all 256 named song slots and verify unchanged root Save suppresses
    the write without a `No changes` dialog. Save-as selection must immediately
    start writing to both empty and occupied destinations without confirmation.
    A clean-state Load must start immediately when its slot is selected. A
    dirty-state Load of an occupied slot must first show a Cancel-first warning
    with `Lose unsaved changes`; an empty slot must skip the warning and report
    empty. Successful Save, Save-as, and Load must
    leave Busy without a completion dialog; Busy/formatting progress and errors
    must remain visible as applicable. Exercise the remaining Copy, Delete, and
    Format confirmations, stored-slot Copy, Delete of the current slot,
    empty-slot feedback, dirty indication, Return cancellation, audio
    fade/pause/resume, USB firmware replacement with data retained, and the
    version/corruption screens. Load v2 and v3 records and verify their in-memory
    migration to a three-minute fully enabled arrangement, with every v2 pad
    following Global, no automatic flash rewrite, and a v4 record after an edit
    followed by Save or after Save-as. Run the
    power-cut matrix in `song-storage.md`; every slot must recover as the old or
    new complete value, never a mixture.
13. Alter every Reset all category and verify its default Cancel choice,
    one-detent nonaccelerated movement, end clamping, and inert Cancel path.
    Create active primaries and forced-fade tails, then confirm Reset. Verify the
    documented defaults including all 2,304 trigger levels restored to 100%, a
    32-frame primary release, existing tails finishing without restart or
    clearing, cleared previews/pulses, preserved brightness/playback/load/
    counters, and return to the root with Reset all highlighted, selection
    empty, and no current song association.
14. Verify idle beat LEDs are off and every selected beat is steady white. Each
    selected trigger/preview indication must mix in its normal palette color.
    With no selection, trigger/preview and Light states must show at their
    normal computed brightness. With a selection, those states on every
    unselected beat must be multiplied to 20% while idle-off keys remain off. Confirm the
    red/yellow/white bottom controls are unaffected by beat dimming.

Then run the combined worst-case workload continuously for 10 minutes. Passing
requires:

- no service or render observation reaching the 5,805 µs deadline;
- DMA launch cadence no greater than 6,168 µs, handoff no greater than 363 µs,
  zero PIO underruns, and GP13 remaining off;
- the UI remaining responsive rather than accumulating seconds of latency;
- after load control has settled, service preferably remaining at or below
  4,350 µs, with deliberate quality shedding visible in the matching counters;
- no invalid catalog accesses or arithmetic wrap; and
- allocation, preview-drop, load-shed, coarse-dither, steal, clipping, and
  tail-overflow counters matching the deliberately exercised cases.

Host tests separately cover deterministic allocation order, exact-frame
coalescing at the maximum due trigger level, dense-start admission, scheduler
equivalence and rollover, 0% visual-only triggers, captured trigger gains,
latest-wins full-gain preview, mute-in-place, independent live gain ramps,
full/coarse dither continuity, adaptive-policy transitions, saturating mix,
catalog rejection, and counter saturation. Pattern tests cover all 256 direct
slots, independent enable/level persistence, relative whole-map adjustment,
clamping, dirty synchronization, and reset defaults. UI host tests cover
ordered chord-entered multi-selection, exclusive single mode, chronological
primary promotion, group replacement, and Pattern entry blocking; atomic group
Beats/Cycle/Sample/Volume edits and mixed-value warning confirmation; primary-
only Sample previews and preview suppression while the encoder is held; clamped
list navigation and persistent pages; per-pad Pattern cursors and confirmations;
context-sensitive master, group, trigger, and whole-map Volume targets; Return
cleanup with preserve-in-mode/clear-at-root selection behavior; held-control
suppression; captured group Mute taps/holds; bounded LED helpers; reset
confirmation, selection-aware beat dimming, musical-state reset, and exact
primary release. Whole-map state tests verify that both `All` and `None` reset
all trigger levels to 100% while `Cancel` leaves enablement and levels intact.
The display-route model covers every root, overlay, prompt, editor, and
confirmation screen; reset-release tests also cover frozen silent gain and
Emergency-policy preservation of primaries and pre-existing tails.
They cannot establish RP2040 XIP, physical input ordering, OLED/LED appearance,
or UI timing. No MacroPad was connected while implementing either the
optimized/adaptive path or the mode-based UI, so the complete hardware run
above remains pending and is the acceptance authority.

## Deferred algorithms

The first 24-sample implementation deliberately keeps playback at each WAV's
native rate and uses one-shot PCM. The following are later, independently
benchmarked features rather than hidden parts of the initial voice engine:

- **Pitch/varispeed and sample-rate conversion.** Start with a fixed-point
  phase accumulator and linear interpolation; evaluate a short polyphase FIR
  if pitch-up aliasing warrants it. The RP2040 has core-local interpolation
  hardware, and Julius O. Smith describes fixed-point windowed-sinc resampling
  in [Digital Audio Resampling](https://www.dsprelated.com/freebooks/pasp/Implementation.html).
- **Attack-hold-decay envelopes.** AHD is meaningful for trigger-only drum
  voices; a complete ADSR has no gate/release event in the current sequencer.
  The [SFZ envelope model](https://sfzformat.com/modulations/envelope_generators/)
  is the compatibility reference.
- **Per-pad filters.** Fixed-point one-pole or biquad tone filters are feasible,
  but coefficient range, smoothing, and UI remain separate work. Use the
  [W3C Audio EQ Cookbook](https://www.w3.org/TR/audio-eq-cookbook/) and
  [Arm Q15 biquad documentation](https://arm-software.github.io/CMSIS-DSP/main/group__BiquadCascadeDF1.html).
- **Round robin and velocity-switched sample layers.** Pattern trigger levels
  already provide linear accents, but choosing among multiple recordings still
  requires explicit sample families and layer thresholds. The SFZ
  [`seq_position`](https://sfzformat.com/opcodes/seq_position/) mechanism is a
  useful behavior reference, but no family is inferred from filenames here.
- **Pitch-preserving time stretch.** WSOLA and phase-vocoder approaches add
  correlation/FFT bursts, buffering, and transient artifacts. Primary sources
  are Verhelst and Roelands' original
  [WSOLA paper](https://www.isca-archive.org/eurospeech_1993/roelands93_eurospeech.html)
  and Flanagan and Golden's
  [phase vocoder paper](https://onlinelibrary.wiley.com/doi/10.1002/j.1538-7305.1966.tb01706.x).
- **Storage compression.** The bank remains direct PCM. IMA ADPCM can be
  reconsidered only if flash becomes constrained; its source specification is
  the Interactive Multimedia Association's
  [Recommended Practices](https://www.cs.columbia.edu/~hgs/audio/dvi/IMA_ADPCM.pdf).

The [RP2040 datasheet](https://datasheets.raspberrypi.com/rp2040/rp2040-datasheet.pdf)
is authoritative for the XIP cache, SRAM banking, interpolators, DMA, and
processor constraints used by this design.
