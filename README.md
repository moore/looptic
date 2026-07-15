# LoopTic

LoopTic is a 12-pad sample sequencer for the [Adafruit MacroPad
RP2040](https://www.adafruit.com/product/5128). The firmware is being ported
from the original CircuitPython [`code.py`](code.py) to Rust using
[Embassy](https://embassy.dev/).

The first Rust milestone embeds the existing kick and open-hi-hat WAV files,
runs twelve independent sequencer voices, and drives the MacroPad keys, rotary
encoder, OLED, and NeoPixels. Keys select a pad; turning the encoder changes
that pad's trigger rate from 0 through 1000 triggers per second. Pads 0–5 use
the kick sample and pads 6–11 use the open hi-hat.

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
`cargo build --release --target thumbv6m-none-eabi`.

Run the platform-independent parser, mixer, scheduler, and UI-state tests on
the host:

```console
cargo test
```

Run the formatting and target checks used before flashing:

```console
cargo fmt --check
cargo clippy --release --target thumbv6m-none-eabi --bin looptic --locked -- -D warnings
```

## Install with UF2/BOOTSEL

Convert the release ELF to UF2:

```console
elf2uf2-rs convert \
  target/thumbv6m-none-eabi/release/looptic \
  looptic.uf2
```

Hold the MacroPad's BOOTSEL button while connecting USB (or press RESET while
holding BOOTSEL), then copy `looptic.uf2` to the mounted `RPI-RP2` volume. The
board reboots into LoopTic after the copy completes.

## Install or debug with probe-rs

Connect an SWD probe to the MacroPad debug pads and run:

```console
cargo flash
```

The `flash` alias selects the RP2040 target, and its Cargo runner is configured
as `probe-rs run --chip RP2040`, so it flashes the ELF and displays `defmt` RTT
output. A probe is optional and is not needed for UF2 installation.

## Hardware verification

After flashing, verify all twelve keys, both encoder directions, the complete
OLED image, each NeoPixel color, and the kick/hat split. Exercise all pads
while turning the encoder and confirm GP13 remains off; a latched red LED means
firmware initialization failed or the audio PIO FIFO stalled. For audio timing
validation, record a rate-1 pattern for 60 seconds and check that it remains
within 0.02% of one trigger per second.

The Rust firmware keeps the two WAV files embedded in flash, so no filesystem
copy step is required. The CircuitPython program and sample files remain in
the repository as the behavior and audio reference.

## Licensing

LoopTic is licensed under the MIT License. The PWM audio encoder and PIO logic
are derived from Raspberry Pi's BSD-3-Clause-licensed `pico-extras`; see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
