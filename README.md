# LoopTic

LoopTic is a nine-pad sample sequencer for the [Adafruit MacroPad
RP2040](https://www.adafruit.com/product/5128). The firmware is being ported
from the original CircuitPython prototype to Rust using
[Embassy](https://embassy.dev/).

The Rust firmware embeds a 24-sample drum bank in flash, provides a fixed pool
of up to 24 overlapping sampler voices, and drives the MacroPad keys, rotary
encoder, OLED, and NeoPixels. Beat pads 0–5 initially use the kick sample and
pads 6–8 initially use the open hi-hat; every pad can select any sample in the
bank. The three bottom-row keys are controls rather than beat pads: logical key
9 (the bottom-left key on GP10) is Mute, logical key 10 (the bottom-middle key
on GP11) is Volume, and logical key 11 (the bottom-right key on GP12) is
Sample. The audio service monitors its own DMA-block deadline and reduces
optional work before CPU pressure can turn into multi-second UI latency or a
PIO underrun.

## Controls and patterns

Hold a beat key and turn the encoder to change that pad's beat division from 0
through 2048 trigger points per base interval. If several keys overlap, the
first (oldest) currently held key is primary. Later keys are ignored while
that key remains held; after it is released, the oldest remaining held key
becomes primary.

While a beat key is held, press the encoder button once to enter pattern mode.
The OLED becomes a scrolling list with one entry per trigger point, and
turning the encoder moves the selection. Subsequent encoder-button presses
toggle the selected trigger off or on. Pattern mode remains active while any
beat key is held and exits after all beat keys are released. A division of 0
displays no triggers, so scrolling and toggling have no effect.

Each pad has a fixed 2048-bit (256-byte) pattern in RAM, initially with every
bit enabled. Editing a trigger at a division below 2048 fills that trigger's
proportional range on the fixed grid. Changing the division does not rescale
or rewrite the stored pattern: playback maps each new trigger range onto the
same grid and samples its first bit. This lets a pattern be viewed at other
divisions without an interpolation step.

With no beat key held, turning the encoder changes the global base interval,
starting at 1000 ms with a 50 ms safety minimum and no application-level
maximum. Holding the encoder button while turning instead adjusts key
NeoPixel brightness from 0 through 100%; brightness starts at 50%. Slow turns
change the interval by 10 ms, a pad division by 1, brightness by 1%, or volume
by 1%; consecutive detents within 40 ms accelerate those changes to 100 ms,
10, 10%, or 10%, respectively. Clockwise increases the interval (slower) and
the selected value, while counter-clockwise decreases it (faster). For
example, a
106,500 ms base interval with pad values 71 and 73 gives a 71:73 polyrhythm
whose 71 side is exactly 40 BPM. The interval uses a saturating `u32`
millisecond value, whose representational limit is about 49.7 days. At extreme
settings such as a 50 ms interval with division 2048, several trigger points
can fall on one audio frame; they intentionally coalesce into one trigger for
that pad on that frame. There is also a hard admission ceiling of eight
scheduled voice starts per pad in each 128-frame audio block. Dense grids keep
advancing their exact clock, pattern, and visual pulses when excess audible
starts are skipped.

## Mute control

Tap the bottom-left Mute key for less than 300 ms to toggle persistent mute;
hold it for at least 300 ms to mute only until release. The cutoff is defined
by `MUTE_TAP_THRESHOLD_MS`, so it can be tuned easily. With no beat key held,
Mute targets the entire sequencer. If one or more beat keys are held, it
targets only the oldest held beat. The target is captured when Mute is pressed
and does not change if the held keys change during that gesture.

Muting suppresses triggers rather than merely silencing their sample output.
Active voices fade out over 32 audio frames (about 1.45 ms), avoiding the click
of an abrupt cut. The clock, pattern position, and visual beat pulses continue
advancing in the background, so unmuting resumes on the next scheduled enabled
trigger without resetting phase.

The Mute key is red at the configured LED brightness. It is bright when the
displayed target is unmuted and dimmed to 20% of that brightness when muted.
With no beat held it displays global mute; while a beat is held it displays
only that beat's own mute state, even if global mute is active.

## Volume control

Hold the bottom-middle Volume key and turn the encoder to adjust volume from
0 through 100%. With no beat key held, this changes the global master volume.
With one or more beat keys held, it changes the oldest held beat's own volume.
The target follows the held keys dynamically, regardless of whether Volume or
the beat key was pressed first. Later beat presses are ignored while a beat is
targeted. Releasing that beat returns to the master even if another beat is
still held; release and press that other beat again to select its local volume.

Master and per-beat volumes are independent linear amplitude percentages,
initially 100%. A beat's effective gain is its own percentage multiplied by
the master percentage; changing the master never overwrites per-beat settings.
Slow encoder turns change the selected volume by 1%, and consecutive detents
within 40 ms change it by 10%. While Volume is held, the OLED displays
`Master N%` or `P# Vol N%` for the live target.

The Volume key is yellow, with its intensity showing the live target's stored
percentage rather than the combined effective gain. Its output is also scaled
by the configured key brightness, and it turns fully off at 0% volume. The
Mute, Volume, and Sample indicators retain their status colors during the
general brightness preview.

Volume targets are sampled at audio-buffer boundaries. Master and every pad
then follow independent 64-frame linear ramps (about 2.90 ms), so changing one
does not reset another ramp. At 0%, voices can still start and advance silently
when capacity is available, so raising the volume can reveal the remaining
tail of a sample. Mute takes precedence: it continues to suppress new triggers
and fades active voices over 32 frames.

## Sample control

Hold the bottom-right Sample key together with a beat key and turn the encoder
to choose that pad's sound from one flat list of 24 samples. The oldest held
beat is the target. Each encoder detent moves exactly one entry and wraps at
both ends; sample browsing never uses encoder acceleration. Volume has control
priority over Sample, and Sample has priority over pattern scrolling. Releasing
Sample restores the mode that was active before it was held.

Every selection automatically requests a preview. If several detents arrive
before the audio task's next control snapshot, only the latest pending preview
is started. A request captured by that snapshot starts at the next buffer
boundary; one arriving just after it waits one additional boundary, so the
safe double-buffered path has less than two buffers (about 11.61 ms) of
worst-case latency. Preview obeys the pad's mute, pad volume, and master
volume, and it pulses that pad's NeoPixel. It does not advance the sequencer
clock or pattern. Scheduled audio has priority if the voice pool is busy, so a
preview may be skipped without affecting the selected sample. Preview is also
the first feature disabled temporarily when the measured audio workload enters
pressure mode.

The OLED shows the sample's short name while browsing. Holding Sample without
a beat displays `Hold beat` and does not edit anything. The Sample key is solid
blue at the configured key brightness; its intensity does not represent the
selected sample.

## Voice behavior and overload protection

Scheduled hits use 24 primary slots without any per-pad partition, so
retriggering a pad does not normally cut off its previous sample and different
pads remain independent. Separate pads that select the same WAV still start
separate voices on the same frame. Only multiple scheduled ticks from the
**same pad** that land on one exact audio frame coalesce into one hit.

When all 24 voices are occupied, the sampler deterministically reuses an older
voice and gives its outgoing sound a 32-frame forced fade. Nine temporary fade
tails keep these transitions click-resistant. Preview has lower allocation
priority than scheduled hits. There are deliberately no choke groups: for
example, a closed hi-hat does not terminate an open hi-hat.

The nominal 128-frame service period is 5,805 µs. The firmware starts shedding
work at 3,775 µs rather than waiting for that deadline: it disables previews,
uses a cheaper duty-equivalent dither pattern, admits at most one scheduled
start per pad per block, stops creating new steal-fade tails, and prevents the
primary pool from growing. At higher pressure it lowers the effective voice
limit and releases excess voices over 32 frames; an emergency can immediately
trim voices and tails. Clock phase, patterns, and visual trigger reports
continue throughout. Quality is restored in stages only after sustained timing
headroom, which avoids oscillating between modes.

LED and OLED refreshes are scheduled from the current time rather than replaying
missed periodic ticks. If audio briefly monopolizes the CPU, stale visual work
is discarded instead of becoming a second UI-latency backlog during recovery.

This adaptive path was added for a specific hardware regression: with every
other pad disabled, the 11,265-frame AKU kick at division 28 and a 1,000 ms
base interval produces about 14–15 simultaneous kick voices. The earlier path
developed seconds of UI latency and distorted audio at that point. The
optimized/adaptive build is designed to degrade voice quality before reaching
that wall; the exact voice capacity still depends on validation on a physical
MacroPad.

## Audio diagnostics

With no beat held, hold Sample and press and hold the encoder button to replace
the normal OLED page with audio diagnostics. `Load N/P/E/R` reports normal,
pressure, emergency, or a recovery stage; `Vactive/limit` shows the last
block's primary-voice peak and the current admission limit. `Svc last/max` is
the complete measured audio service time, `Ren last/max` is the renderer alone,
`DMA` is the maximum launch cadence, and `Late`/`U` are service-deadline misses
and PIO underruns. Release either control to return to the normal display.

The firmware also retains high-water marks and cumulative counters for load
transitions, the minimum effective voice limit, coarse-dither frames, skipped
previews and triggers, discarded fade tails/primaries, clipping, steals, and
voice peaks. GP13 still latches only for an actual underrun or fatal hardware
or initialization error; entering pressure mode by itself is not a fault.

For allocation rules, mixing order, resource limits, diagnostics, failure
modes, and the hardware benchmark contract, see the
[sampler architecture](docs/sampler-architecture.md).

## Audio hardware

The supported external audio hardware is the
[MacroPadSynthPlug](https://github.com/todbot/macropadsynthplug). It is a
passive PWM filter rather than an I2S codec: firmware emits dithered PWM on
STEMMA SDA/GP20 and the plug presents the filtered signal on both channels of
its 3.5 mm jack. STEMMA SCL/GP21 is reserved by the plug for MIDI input and is
not used by this milestone.

The jack is **line level**. Connect it to powered speakers, an amplifier, an
audio interface, or another line input; it cannot drive a passive speaker
directly. The MacroPad's built-in speaker is intentionally held disabled.

## Pin and peripheral assignments

| Function | MacroPad pins | RP2040 resource |
| --- | --- | --- |
| SynthPlug audio | GP20 / STEMMA SDA | PIO0 SM0, DMA CH0 |
| 12 NeoPixels | GP19 | PIO1 SM0, DMA CH1 |
| Rotary encoder | GP17 and GP18 | PIO1 SM1 |
| Encoder push switch | GP0, active low | Debounced GPIO input |
| Beat keys 1–9 | GP1–GP9, active low | GPIO inputs with pull-ups |
| Mute / Volume / Sample | GP10 / GP11 / GP12, active low | GPIO inputs with pull-ups |
| OLED clock/data | GP26 SCK, GP27 MOSI | SPI1 at 10 MHz |
| OLED control | GP22 CS, GP23 reset, GP24 D/C | GPIO outputs |
| Built-in speaker shutdown | GP14 | Held low |
| Status LED | GP13 | Fatal error or audio underrun latch |
| SynthPlug MIDI input | GP21 / STEMMA SCL | Reserved, not configured |

See Adafruit's [MacroPad pinout](https://learn.adafruit.com/adafruit-macropad-rp2040/pinouts)
for the full board layout.

## Prerequisites

- A stable Rust toolchain managed by
  [rustup](https://rustup.rs/). `rust-toolchain.toml` automatically selects
  stable Rust and installs `thumbv6m-none-eabi`, Clippy, and rustfmt.
- [`elf2uf2-rs`](https://github.com/JoNil/elf2uf2-rs) for creating a UF2 file:
  `cargo install elf2uf2-rs`.
- [`cargo-bloat`](https://github.com/RazrFalcon/cargo-bloat) for the documented
  firmware size report: `cargo install cargo-bloat`.
- Optional: [`probe-rs`](https://probe.rs/) and an SWD probe for flashing and
  viewing RTT logs without using BOOTSEL.

## Build and test

Build the RP2040 release image:

```console
cargo firmware
```

This is an alias for
`cargo build --release`. The project defaults all build commands to the
`thumbv6m-none-eabi` firmware target.

Inspect the firmware's code-size breakdown with `cargo-bloat`:

```console
cargo firmware-bloat
```

The project alias supplies the release profile, RP2040 target, and firmware
binary automatically. Additional `cargo-bloat` options can be appended, such
as `cargo firmware-bloat --crates`.

Run the platform-independent parser, mixer, scheduler, and UI-state tests on
the host:

```console
cargo host-test
```

The alias selects `x86_64-unknown-linux-gnu`, overriding the embedded default
so Rust's standard test harness is available.

Run the formatting and target checks used before flashing:

```console
cargo fmt --check
cargo clippy --release --target thumbv6m-none-eabi --bin looptic --locked -- -D warnings
```

## Flash over USB

Put the MacroPad into its USB bootloader by holding BOOTSEL while connecting
USB, or by holding BOOTSEL while pressing RESET. Once the `RPI-RP2` volume is
mounted, run:

```console
cargo flash
```

The `flash` alias builds the optimized RP2040 ELF, and the configured
`elf2uf2-rs deploy` runner converts and copies it to the USB bootloader. The
board reboots into LoopTic when deployment completes. No SWD probe is needed.

If the separate `probe-rs` `cargo-flash` subcommand is installed, Cargo may
warn that this project's `flash` alias shadows it. The alias is intentional;
`cargo run --release` performs the same USB deployment without that naming
collision.

## Create a UF2 manually

Convert the release ELF to UF2:

```console
elf2uf2-rs convert \
  target/thumbv6m-none-eabi/release/looptic \
  looptic.uf2
```

Copy `looptic.uf2` to the mounted `RPI-RP2` volume. This is equivalent to the
automated `cargo flash` workflow.

## Install or debug with probe-rs

Connect an SWD probe to the MacroPad debug pads and run:

```console
cargo firmware
probe-rs run --chip RP2040 target/thumbv6m-none-eabi/release/looptic
```

This flashes the ELF and displays `defmt` RTT output. A probe is optional and
is not needed for USB deployment.

## Hardware verification

After flashing, verify all nine beat keys, both encoder directions, the
complete OLED image, each beat NeoPixel color and brightness range, and the
initial kick/hat split across pads 0–5 and 6–8. Confirm that the bottom row is
Mute, Volume, and Sample from left to right; none should create sequencer
beats. Mute must remain red, Volume yellow, and Sample solid blue during the
general brightness preview.

Hold overlapping beat keys and verify that only the oldest held key is edited
until it is released. Enter pattern mode, scroll its OLED list, toggle several
entries, and verify that the mode persists across a change of primary key but
exits once every beat key is released. Check that division 0 shows no trigger
entries, that patterns start fully enabled after boot, and that edits made at
one division are sampled consistently after changing the division. Verify
that holding the encoder button changes brightness only when no pad is
selected.

Test short Mute taps and 300 ms holds both globally and with a beat held;
verify that the gesture keeps its original target if held keys change. Confirm
the Mute LED is bright red when its displayed target is unmuted, is dimmed to
20% when muted, and shows only the selected beat's local setting while a beat
is held. Muted voices should fade cleanly in about 1.45 ms while their visual
pulses and phase continue, then resume at the next scheduled trigger after
unmuting.

Hold Volume and verify both chord orders, first-beat targeting, fallback to
master without promoting an already-held secondary beat, 1%/10% adjustment,
OLED `Master N%` and `P# Vol N%` feedback, and the full 0–100% gain range.
Confirm the yellow indicator follows the selected stored setting and
configured LED brightness, turns off at 0%, and does not replace the red or
blue indicators. Verify master and per-beat gains multiply, independent
64-frame ramps remove abrupt gain steps, voices can continue silently at zero,
and Mute still suppresses triggers.

Hold Sample with each beat and browse all 24 names. Verify one entry per
detent, wraparound in both directions, no acceleration, automatic preview
within the documented two-buffer worst case, and restoration of the previous
pattern or division mode on release. Rapidly cross several entries and confirm
only the latest pending preview starts. With no beat held, Sample must show
`Hold beat` without editing. Confirm Volume overrides Sample, Sample overrides
pattern scrolling, and preview follows mute and both gains, pulses the target
pad, and does not move its pattern or clock.

Create overlapping tails and verify repeated hits do not normally cut each
other off. Two pads using the same sample must still start separate voices on
the same frame, while only multiple ticks from one pad on one exact frame
coalesce. Verify open and closed hats overlap because no choke groups are
enabled. Drive more than 24 overlapping starts to exercise deterministic
stealing and its 32-frame forced fades without confusing pool pressure with an
audio underrun.

Exercise all beat pads while browsing samples, turning the encoder, updating
patterns, muting, and changing volume for 10 minutes. Inspect the diagnostics
page during the run. Require zero 5,805 µs audio-service deadline misses, zero
PIO underruns, and GP13 remaining off; a latched red LED means initialization
or real-time audio failed. The preferred settled maximum service time is at or
below 4,350 µs, with load shedding visible rather than accumulating UI work.
A deliberately forced fade-tail overflow is separately diagnostic and may
shorten an existing fade. At the 50 ms/2048 extreme, same-pad coalescing and
the per-block start limit are expected and are not underruns.

Reproduce the original overload case separately: disable pads 2–9, select AKU
Kick for pad 1, set a 1,000 ms base interval, and sweep its division through 28
and beyond. The UI must remain responsive, audio may degrade according to the
documented load ladder, and the deadline/underrun counters must remain zero.
This acceptance check has not yet been run on the optimized/adaptive firmware
because no MacroPad is connected to the development environment. For timing
accuracy, also leave a `Beat 1` pattern enabled, record it for 60 seconds, and
check that it remains within 0.02% of one trigger per second.

All 24 WAV files are embedded in XIP flash, so no filesystem copy step is
required at runtime.

## Licensing

LoopTic is licensed under the MIT License. The PWM audio encoder and PIO logic
are derived from Raspberry Pi's BSD-3-Clause-licensed `pico-extras`; see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
