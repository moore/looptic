# LoopTic

LoopTic is a 12-pad sample sequencer for the [Adafruit MacroPad
RP2040](https://www.adafruit.com/product/5128). The firmware is being ported
from the original CircuitPython prototype to Rust using
[Embassy](https://embassy.dev/).

The first Rust milestone embeds the existing kick and open-hi-hat WAV files,
runs twelve independent sequencer voices, and drives the MacroPad keys, rotary
encoder, OLED, and NeoPixels. Keys select a pad; turning the encoder changes
that pad's beat multiplier from 0 through 1000 triggers per base interval.
With no key held, the encoder changes the global base interval, starting at
1000 ms with a 50 ms safety minimum and no application-level maximum. Slow
turns change the interval by 10 ms and pad multipliers by 1; consecutive
detents within 75 ms accelerate both controls by 10x. Clockwise increases the
interval (slower), while counter-clockwise decreases it (faster). For example,
a 106,500 ms base interval with pad values 71 and 73 gives a 71:73 polyrhythm
whose 71 side is exactly 40 BPM. Pads 0–5 use the kick sample and pads 6–11 use
the open hi-hat. The interval uses a saturating `u32` millisecond value, whose
representational limit is about 49.7 days.

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
| Keys 1–12 | GP1–GP12, active low | GPIO inputs with pull-ups |
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

After flashing, verify all twelve keys, both encoder directions, the complete
OLED image, each NeoPixel color, and the kick/hat split. Exercise all pads
while turning the encoder and confirm GP13 remains off; a latched red LED means
firmware initialization failed or the audio PIO FIFO stalled. For audio timing
validation, set the base interval to 1000 ms, record a `Beat 1` pattern for 60
seconds, and check that it remains within 0.02% of one trigger per second.

The Rust firmware keeps the two WAV files embedded in flash, so no filesystem
copy step is required. The sample files remain in the repository as the audio
reference.

## Licensing

LoopTic is licensed under the MIT License. The PWM audio encoder and PIO logic
are derived from Raspberry Pi's BSD-3-Clause-licensed `pico-extras`; see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
