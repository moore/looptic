# LoopTic

LoopTic is a nine-pad sample sequencer for the [Adafruit MacroPad
RP2040](https://www.adafruit.com/product/5128). The firmware is being ported
from the original CircuitPython prototype to Rust using
[Embassy](https://embassy.dev/).

The first Rust milestone embeds the existing kick and open-hi-hat WAV files,
runs nine independent sequencer voices, and drives the MacroPad keys, rotary
encoder, OLED, and NeoPixels. Beat pads 0–5 use the kick sample and pads 6–8
use the open hi-hat. The three bottom-row keys are controls rather than beat
pads: logical key 9 (the bottom-left key on GP10) is Mute, logical key 10 (the
bottom-middle key on GP11) is Volume, and key 11 is reserved and remains off.

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
by 1%; consecutive detents within 75 ms accelerate those changes to 100 ms,
10, 10%, or 10%, respectively. Clockwise increases the interval (slower) and
the selected value, while counter-clockwise decreases it (faster). For
example, a
106,500 ms base interval with pad values 71 and 73 gives a 71:73 polyrhythm
whose 71 side is exactly 40 BPM. The interval uses a saturating `u32`
millisecond value, whose representational limit is about 49.7 days. At extreme
settings such as a 50 ms interval with division 2048, several trigger points
can fall on one audio frame; they intentionally coalesce into one trigger for
that pad on that frame.

## Mute control

Tap the bottom-left Mute key for less than 300 ms to toggle persistent mute;
hold it for at least 300 ms to mute only until release. The cutoff is defined
by `MUTE_TAP_THRESHOLD_MS`, so it can be tuned easily. With no beat key held,
Mute targets the entire sequencer. If one or more beat keys are held, it
targets only the oldest held beat. The target is captured when Mute is pressed
and does not change if the held keys change during that gesture.

Muting suppresses triggers rather than merely silencing their sample output.
The clock, pattern position, and visual beat pulses continue advancing in the
background, so unmuting resumes on the next scheduled enabled trigger without
resetting phase.

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
within 75 ms change it by 10%. While Volume is held, the OLED displays
`Master N%` or `P# Vol N%` for the live target.

The Volume key is yellow, with its intensity showing the live target's stored
percentage rather than the combined effective gain. Its output is also scaled
by the configured key brightness, and it turns fully off at 0% volume. The
Mute and Volume indicators retain their red and yellow status colors during
the general brightness preview; reserved key 11 remains off.

Volume changes are applied on audio-buffer boundaries without smoothing. At
0%, voices still start and advance silently, so raising the volume can reveal
the remaining tail of a sample. Mute takes precedence: it continues to
suppress new triggers and stop active voices. Gain changes may therefore click
and must be checked on the target audio hardware.

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
| Mute / Volume / reserved | GP10 / GP11 / GP12, active low | GPIO inputs with pull-ups |
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
kick/hat split across pads 0–5 and 6–8. Confirm that the bottom-left key is
Mute, the bottom-middle key is Volume, and only bottom-right key 11 remains
reserved and off; none of them should create sequencer beats.
Hold overlapping keys and verify that only the oldest held key is edited until
it is released. Enter pattern mode, scroll its OLED list, toggle several
entries, and verify that the mode persists across a change of primary key but
exits once every beat key is released. Check that division 0 shows no trigger
entries, that patterns start fully enabled after boot, and that edits made at
one division are sampled consistently after changing the division. Verify
that holding the encoder button changes brightness only when no pad is
selected. Test short Mute taps and 300 ms holds both globally and with a beat
held; verify that the gesture keeps its original target if held keys change.
Confirm the Mute LED is bright red when its displayed target is unmuted, is
dimmed to 20% when muted, and shows only the selected beat's local setting
while a beat is held. Verify that muted beats stop sounding immediately while
their visual pulses and phase continue, then resume at the next scheduled
trigger after unmuting. Hold Volume and verify both chord orders, first-beat
targeting, fallback to master without promoting an already-held secondary
beat, 1%/10% adjustment, OLED
`Master N%` and `P# Vol N%` feedback, and the full 0–100% gain range. Confirm
the yellow indicator follows the selected stored setting and configured LED
brightness, turns off at 0%, and does not replace the red Mute indicator.
Verify master and per-beat gains multiply, voices continue silently at zero,
and Mute still suppresses triggers. Listen for clicks during rapid,
block-aligned changes because gain smoothing is intentionally absent.
Exercise all beat pads while turning the encoder, updating patterns, muting,
and changing volume, and confirm GP13 remains off; a latched red LED means
firmware initialization failed or the audio PIO FIFO stalled. At the 50
ms/2048 extreme, coalesced triggers are expected and are not an underrun. For
audio timing validation, set the base interval to 1000 ms, leave a `Beat 1`
pattern enabled, record it for 60 seconds, and check that it remains within
0.02% of one trigger per second.

The Rust firmware keeps the two WAV files embedded in flash, so no filesystem
copy step is required. The sample files remain in the repository as the audio
reference.

## Licensing

LoopTic is licensed under the MIT License. The PWM audio encoder and PIO logic
are derived from Raspberry Pi's BSD-3-Clause-licensed `pico-extras`; see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
