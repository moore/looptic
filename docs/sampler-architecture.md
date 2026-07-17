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
                 voice fade x independent pad-gain ramp
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

### Exact-frame coalescing

Only multiple scheduled ticks belonging to the **same pad** and landing on the
same output frame coalesce into one request. This bounds extreme settings such
as a 50 ms base interval divided into 2048 points.

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
the 24-slot pool remains polyphonic without allowing an extreme grid to run as
many as 128 allocations per pad in one service cycle.

### Primary pool

The sampler owns 24 primary slots without per-pad partitioning. There is no
one-voice-per-pad limit, so repeated hits and long tails overlap naturally and
one pad may occupy every available slot. Each primary records its pad, captured
sample, cursor, start age, and gain/fade state. Preview and scheduled starts
share the same playback representation after allocation.

For a scheduled audible request, allocation is deterministic:

1. Use a free primary slot.
2. Otherwise steal the oldest primary owned by the requesting pad.
3. Otherwise steal the oldest primary whose master × pad target gain is zero.
4. Otherwise steal the globally oldest primary.

A zero-volume scheduled request may use a free slot and advance silently. If
the pool is full, it may reuse only a victim whose **current ramped effective
gain** has reached zero; otherwise it is dropped instead of stealing an
audible voice. Conversely, an audible request may select a target-zero victim
that is still ramping down, because the forced tail below preserves its
remaining sound without a hard cut.

Preview is deliberately lower priority. It uses a free primary slot, or may
replace the oldest voice from the previewed pad; if neither is available, the
preview drops. It never steals another pad's audible voice. Rapid browsing
uses a latest-wins mailbox: all detents update the stored selection, but only
the newest pending preview at a block boundary is considered.

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

Master and per-pad gains remain independent linear percentages. Each stores a
current and target value and traverses a 64-frame linear ramp, approximately
2.90 ms, whenever its target changes. Pad and master ramps run independently
and multiply; they are not collapsed into one effective target. A new change
starts from the ramp's current value.

Each rendered sample is processed in this order:

1. Read signed PCM from the voice's captured sample and advance its cursor.
2. Apply any 32-frame voice fade and the owning pad's 64-frame gain ramp.
3. Sum all 24 primaries and nine temporary tails in signed `i32`.
4. Apply the independent 64-frame master ramp to the sum.
5. Saturate once to `i16`, then feed the existing stateful PWM dither encoder.

No compressor, limiter, normalization, or automatic gain compensation is
applied. Deliberately loud combinations may saturate; they must never wrap.

Automatic preview uses the same PCM, mute, pad-volume, master-volume, mixing,
and allocation paths as scheduled audio. It also pulses the selected pad's
NeoPixel. Preview publishes only that visual pulse; it does not advance the
sequencer clock or pattern or change phase. A muted pad does not produce an
audible preview.

### Real-time implementation choices

The release profile uses `opt-level = 3`. On ARM, `Sequencer::render` and the
deterministic voice allocator are linked into `.data.ram_func` and copied to
SRAM at startup. PCM remains in XIP flash, but fetching these hot paths from
SRAM prevents their instructions from competing with simultaneous voice sample
reads for the same XIP cache/port.

The mixer uses signed `i32` fixed-point arithmetic. Exact 0% and 100% gain
paths avoid multiplication and division; other per-voice gains, 32-frame
fades, and 64-frame ramps use bounded power-of-two operations. `i32` is also a
proven-safe accumulator for all 24 primaries plus nine tails, and the master
gain is applied only once after that sum. Inactive slots are rejected before
sample lookup or gain work, active counts are maintained incrementally, and
allocation-state arrays are constructed only on frames with a scheduled or
preview request.

Normal contiguous scheduling uses a rational deadline accumulator: wide
division is confined to a timing change or non-contiguous recovery, while the
frame hot path advances with additions and comparisons. Pattern first-bit
positions are advanced incrementally without a variable divide. These changes
preserve the original ceiling-based global grid and 2048-bit pattern mapping;
they change cost rather than musical phase.

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
- Zero-volume scheduled drops.
- Previews dropped by mute or the lower-priority allocation rule. Superseded
  latest-wins requests are observable from the mailbox's replacement return
  value when instrumenting control behavior.
- Forced-tail high-water mark and `fade_tail_overflow`.
- Load-shed previews, scheduled starts, fade tails, and primaries, plus the
  number of frames encoded with coarse dither.
- Output frames that required `i16` saturation.
- Catalog initialization faults.

On the device, hold Sample plus the encoder push switch with no beat held (and
without Volume) to open the OLED diagnostics page. `Load N/P/E/R` identifies
Normal, Pressure, Emergency, or any recovery stage. `Vx/y` is the last block's
primary peak and current limit, `Svc` and `Ren` show last/maximum microseconds,
`DMA` shows maximum launch cadence, and `Late`/`U` show service-deadline misses
and underruns. Release either control to return to the ordinary Sample/base
display. Less frequently needed counters remain in `SharedState` and
`SamplerDiagnostics` for RTT/debug instrumentation.

Only a PIO FIFO stall increments the underrun count and latches GP13. A service
or render that exceeds 5,805 µs increments its separate deadline-miss counter;
it is valuable diagnostic evidence but is not itself reported as a hardware
underrun. Catalog validation and OLED initialization/flush failures are fatal
and also latch GP13, while NeoPixel work remains outside the audio refill path.
`SampleId` construction is checked, so an invalid index cannot reach the
real-time catalog lookup. Counters saturate instead of wrapping.

Pool exhaustion is expected behavior, not an underrun: scheduled audible hits
use the steal policy, zero-volume hits may drop, and preview may drop. Tail
overflow is also recoverable but diagnostic because it can expose a click.
Output saturation and deliberate load shedding are recoverable quality events,
distinct from timing failure.

## Resource budget

| Resource | Budget or baseline |
| --- | ---: |
| RP2040 flash layout | 8 MiB |
| Current release flash use | 1,156,944 bytes |
| Complete 24-sample bank | 1,064,964 bytes |
| Free flash in 8 MiB layout | 7,231,664 bytes (about 6.90 MiB) |
| Current linked static RAM span | 29,728 bytes |
| RAM remaining before runtime stack | 240,608 bytes (about 235.0 KiB) |
| RP2040 SRAM | 264 KiB |
| Primary voices | 24 physical slots; adaptive effective limit 1–24 |
| Temporary steal tails | up to 9 fixed slots; adaptive limit 0 or 9 |
| Nominal audio-service deadline | 5,805 µs |
| Joined-FIFO reserve | 363 µs / 8 frames |
| Normal-to-pressure threshold | 3,775 µs service/EWMA |
| Pressure voice-reduction threshold | 4,350 µs service |
| Emergency threshold | 5,225 µs service |

These values come from the current optimized release ELF's loadable sections:
flash is 256 bytes boot2 + 192 bytes vectors + 70,368 bytes text + 1,072,008
bytes read-only data + the 14,120-byte data load image. Static RAM is 14,120
bytes data + 14,584 bytes BSS + the 1,024-byte reserved uninitialized region.
The data section includes the 11,740-byte hot renderer and 2,216-byte voice
allocator copied to SRAM; their load images also consume flash. PCM consumes
no SRAM beyond catalog references.
Voice pools and diagnostics are fixed-size static state, and the firmware
continues to have no heap. The RAM remainder is available to the runtime stack
and does not claim a measured worst-case stack high-water mark. ELF and UF2
file lengths are not device-usage figures because they include debug
information or container formatting.

## Hardware benchmark protocol

Benchmark the release profile on a MacroPad RP2040 at the default 125 MHz, with
the actual PIO/DMA PWM path and SynthPlug connected. Use the built-in RP2040
timer observations and OLED diagnostics; retain both renderer-only and complete
service maxima, DMA cadence, handoff delay, load transitions, effective voice
limit, and underruns.

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
   and operate the encoder and keys while watching `Load`, `Svc`, `Ren`, `DMA`,
   `Late`, and `U`.
2. Twenty-four overlapping long primaries reading widely separated parts of
   the XIP catalog.
3. Continuous scheduled pressure that forces steals and first keeps all nine
   fade tails active, then verifies that Pressure stops adding tails and
   Emergency removes them.
4. Global and per-pad mute while the primary pool is full.
5. Simultaneous independent master and pad gain ramps.
6. Fast Sample-key browsing combined with scheduled hits, verifying that
   Pressure drops previews before essential clock work.
7. All nine pads at dense rates while rotating the encoder and refreshing the
   OLED and NeoPixels; verify the eight-start normal quota and one-start
   pressure quota without clock or visual-phase drift.
8. A dedicated tail-overflow run that verifies the deterministic replacement
   rule and counter without misclassifying it as a DMA underrun.
9. A recovery run long enough to observe Emergency/Pressure, voice-limit
   increments, RecoveryDither, RecoveryTails, RecoveryStarts, and Normal in
   order without chattering.

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
coalescing, dense-start admission, scheduler equivalence and rollover,
latest-wins preview, mute-in-place, independent gain ramps, full/coarse dither
continuity, adaptive-policy transitions, saturating mix, catalog rejection,
and counter saturation. They cannot establish RP2040 XIP or UI timing. No
MacroPad was connected while implementing this optimized/adaptive path, so the
hardware run above remains the acceptance authority.

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
- **Round robin and accent/velocity layers.** These require explicit sample
  families and pattern velocity state. The SFZ
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
