# LoopTic

LoopTic is a nine-voice drum sequencer and song arranger for the
[Adafruit MacroPad RP2040](https://www.adafruit.com/product/5128) and
[MacroPadSynthPlug](https://github.com/todbot/macropadsynthplug). It turns the
MacroPad's keys, encoder, OLED, and NeoPixels into a compact hardware instrument
with per-voice rhythms, editable patterns, a finite Tracks arrangement, and
flash-backed song storage.

The firmware is written in `no_std` Rust with Embassy. Audio is generated as
22,050 Hz PWM through RP2040 PIO and DMA, then filtered to stereo line level by
the SynthPlug.

## Highlights

- Nine independently timed voices and 24 embedded drum samples.
- Per-voice Beats, Cycle length overrides, 256-slot Patterns, accents, and
  Pattern Cycles.
- A vertical nine-column Tracks arranger with zoom, transport, span painting,
  and live audition over the stored arrangement.
- Ordered multi-voice selection for batch Beats, Cycle length, Sample, Volume,
  and Mute edits.
- Click-resistant gain ramps, 24 overlapping sample voices, deterministic
  stealing, and adaptive load control.
- 256 named song slots in a power-loss-tolerant journal in the final 2 MiB of
  the MacroPad's flash.
- Host-tested sequencing, UI, codec, and storage behavior with a heap-free
  firmware runtime.

## Hardware

You need:

- an Adafruit MacroPad RP2040;
- a MacroPadSynthPlug connected to the STEMMA port;
- a USB data cable; and
- powered speakers, an amplifier, an audio interface, or another line input.

The SynthPlug output is line level and cannot drive a passive speaker directly.
LoopTic keeps the MacroPad's built-in speaker disabled. GP20/STEMMA SDA carries
audio; GP21/STEMMA SCL is reserved for the SynthPlug's MIDI input but is not
currently used by LoopTic.

## Getting started

Install [Rust through rustup](https://rustup.rs/) and the UF2 deployment tool:

```console
cargo install elf2uf2-rs
```

Clone the repository if needed:

```console
git clone https://github.com/moore/looptic.git
cd looptic
```

The checked-in `rust-toolchain.toml` selects stable Rust and installs the
`thumbv6m-none-eabi` target, Clippy, and rustfmt. From the repository root,
build the firmware and run the platform-independent tests:

```console
cargo host-test --locked
cargo firmware --locked
```

`cargo firmware` is the project alias for the optimized RP2040 release build.
The `host-test` alias selects `x86_64-unknown-linux-gnu`; on another host, use
`cargo test --no-default-features --target <your-host-triple> --locked`.

To install over USB:

1. Hold BOOTSEL while connecting the MacroPad, or hold BOOTSEL while pressing
   RESET.
2. Wait for the `RPI-RP2` volume to mount.
3. Run:

   ```console
   cargo run --release --locked
   ```

The board reboots into LoopTic after deployment. Normal LoopTic UF2 updates do
not address the reserved song partition, so saved songs are preserved. A
whole-chip or mass erase performed by a debugger will erase them.

The repository also retains `cargo flash` as a short alias. The explicit
`cargo run` form above avoids Cargo's name-collision warning when the unrelated
external `cargo-flash` subcommand is installed.

## First rhythm

LoopTic boots with transport running, a one-second global Cycle length, and all
voice Beats set to zero, so it is initially silent.

1. Tap voice key 1. The key turns white to show that it is selected.
2. Leave `Beats` highlighted and press the encoder.
3. Turn the encoder to `4`. Voice 1 now triggers four times per one-second
   Cycle.
4. Press Return to go back to the root menu.
5. Open `Sample` to choose a sound, or `Pattern` to enable, disable, and accent
   individual trigger slots.

Turn the encoder to navigate and press it to enter or confirm. Return closes an
editor or returns toward the root. Hold Volume and turn the encoder for master
or selected-voice volume. Mute normally toggles or momentarily mutes its target;
inside Tracks it becomes the blue Play/Pause control.

See the [user guide](docs/user-guide.md) for selection rules, every screen,
Tracks editing, saving, loading, and complete control behavior.

## Development commands

Run host tests:

```console
cargo host-test --locked
```

Run the formatting and lint checks used for firmware changes:

```console
cargo fmt --check
cargo clippy --target x86_64-unknown-linux-gnu --no-default-features --all-targets --locked -- -D warnings
cargo clippy --release --target thumbv6m-none-eabi --bin looptic --locked -- -D warnings
```

Build the release firmware:

```console
cargo firmware --locked
```

Install `cargo-bloat` and inspect release size by crate when needed:

```console
cargo install cargo-bloat
cargo firmware-bloat --crates
```

To create a UF2 without deploying it:

```console
elf2uf2-rs convert \
  target/thumbv6m-none-eabi/release/looptic \
  looptic.uf2
./scripts/check-firmware-layout.sh
```

The layout check verifies that neither the ELF nor the UF2 reaches the song
partition. It requires Bash, `readelf`, `awk`, and Perl. Copy `looptic.uf2` to
`RPI-RP2` to install it manually.

An SWD probe is optional. After `cargo firmware`, a typical debug launch is:

```console
probe-rs run --chip RP2040 target/thumbv6m-none-eabi/release/looptic
```

Check the probe tool's erase policy before using it on a unit with saved songs.

## Documentation

- [User guide](docs/user-guide.md): musical model, controls, UI flows, every
  screen, Tracks editing, and song management.
- [Internals](docs/internals.md): source layout, task ownership, state flow,
  sequencing, rendering, persistence, and verification strategy.
- [Sampler architecture](docs/sampler-architecture.md): detailed audio engine,
  timing, allocation, load-control, resource budget, and hardware benchmark
  contract.
- [Song storage architecture](docs/song-storage.md): flash geometry, version
  gates, journaling, migrations, and power-loss behavior.
- [Sample provenance](samples/README.md): sample sources, format, and catalog
  ordering.

## License

LoopTic is licensed under the MIT License. The PWM audio encoder and PIO logic
include BSD-3-Clause-derived work from Raspberry Pi's `pico-extras`; the drum
samples are distributed under CC0 with retained provenance. See
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
