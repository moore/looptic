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
Return. The audio service monitors its own DMA-block deadline and reduces
optional work before CPU pressure can turn into multi-second UI latency or a
PIO underrun.

## Navigation and beat selection

The OLED root menu is ordered `Beats`, `Cycle length`, `Pattern`, `Sample`,
`Light`, `Save`, `Songs`, and `Reset all`, with Beats highlighted at boot. Each
encoder detent moves one item, clamps at Beats and Reset all, and never
accelerates; pressing the encoder enters the selected mode or invokes Save. The
five visible rows scroll as the cursor reaches the later entries. The root
cursor is remembered when leaving and returning to the menu. Beats, Cycle
length, Pattern, Sample, Light, and Songs remain open until Return is pressed;
the Reset all confirmation instead exits when either choice is confirmed.

Pressing a beat key toggles a persistent selection. Pressing an unselected beat
selects it and replaces the previous selection; pressing the selected beat
again clears it. Releasing a beat key does nothing. Selection works at the root
and inside every mode, so a pad does not need to remain held while it is edited.
The current UI selects at most one pad. Its nine-bit storage and mask operations
can represent multiple pads, but editing and control routing remain exclusive;
future multi-select modes will still need their own routing policy.
Changing or clearing the beat selection does not preview a sample.

The bottom-right Return key is white. Pressing it in a mode or confirmation
normally returns to the root while preserving the selected beat and leaving the
root cursor on its remembered item. Pressing Return while already at the
root clears the beat selection. On the scan that recognizes its press, Return
takes priority over other key-press edges, an encoder-button press, and a
simultaneous Mute release, which is cancelled without toggling. The physical
Return level also blocks encoder detents throughout its debounce window. Return
first closes an open Pattern Cycles editor while remaining in Pattern; pressing
it again returns to the root. Otherwise, Return cancels any still-uncommitted
choice or control gesture and ignores
controls that were already physically held until they have been released; this
prevents an input intended for the prior screen from taking effect on the root.

## Pattern mode

Without a selected beat, Pattern displays `Select voice`. With a beat selected,
the OLED shows that pad's scrolling list: `Cycles`, `All`, then one entry for
each active trigger point. `Cycles` ranges from 1 through the largest value
whose beat count fits in 256 slots; 3 beats at 2x exposes 6 triggers without
changing the three-ticks-per-interval cadence. Push Cycles to edit it, turn one
step at a time, and push or Return to finish. Every trigger row shows its
`ON`/`off` state and stored
0–100% trigger level; `All` shows the whole map's rounded average level. Slow
turns move one row; consecutive detents on the same setting and in the same
direction within 40 ms move ten rows. The list clamps at `Cycles` and the final
visible trigger, so accelerated movement cannot cross an endpoint. Pressing the
encoder toggles an individual trigger. Each pad
remembers its own list cursor when selection moves between pads.

Selecting `All` opens `Cancel`, `All`, and `None`, with `Cancel` selected by
default. Turn the encoder to choose; the choice clamps at `Cancel` and `None`.
Press the encoder to confirm and return to the pattern list. `All`
fills the complete map and `None` clears it; either committed whole-map choice
also resets all 256 stored trigger levels to 100%. `Cancel` and Return preserve
both the enable map and trigger levels. At division 0, `Cycles` and `All`
remain available.

Each pad has 256 persistent pattern slots. A 256-bit (32-byte) enable map starts
with every trigger on, and each slot separately stores a trigger level that
starts at 100%. A division of `n` repeated `r` times reads and edits the first
`n × r` slots directly:
trigger 1 uses slot 0, trigger 2 uses slot 1, and so on. Reducing the division
hides later slots without erasing either their enable state or level; increasing
it reveals their previous settings. Normal division changes and individual
edits never rewrite hidden slots.

The `All` entry reports `All ON`, `All off`, or `All mix`. Its confirmation
choice acts on the entire 256-bit enable map, including slots hidden by the
current division: `All` fills every bit, `None` clears every bit, and `Cancel`
changes nothing. Committing either `All` or `None` resets every stored trigger
level, including hidden levels, to 100%; `Cancel` leaves them unchanged. This
makes both the enablement and level result explicit regardless of the map's
current state. `All` also works at division 0.

Hold Volume while a trigger row is highlighted to adjust that slot's level.
Slow turns add or subtract one percentage point; same-target, same-direction
detents within 40 ms use ten-point steps. Holding Volume on `All` applies that
same relative delta to all 256 levels, including hidden and disabled slots,
rather than setting them to one value. Accents therefore keep their differences
until individual values clamp independently at 0% or 100%. The OLED changes to
`P# T### N%` for one trigger or `P# All avg N%` for the rounded whole-map
average while the modifier is held.

## Beats mode

With no beat selected, Beats displays `Select voice` and encoder turns do
nothing. With a pad selected, turning the encoder directly changes that pad's
Beats value from 0 through 256 trigger points per effective Cycle. Slow turns
change it by 1; consecutive same-target, same-direction detents within 40 ms
change it by 10. Clockwise increases Beats and counter-clockwise decreases it.
Encoder push has no action, and Return goes directly to the root.

## Cycle length mode

With no beat selected, turning the encoder directly changes the global Cycle
length, starting at 1000 ms with a 50 ms safety minimum and no application-level
maximum. With a selected pad, turning the encoder directly changes that pad's
Cycle length. `Length 0 (Global)` follows the current global value; any value at
or above 50 ms is an independent persistent override. Values 1 through 49 are
skipped: clockwise from 0 selects 50 ms, and counter-clockwise below 50 ms
returns to 0.

Slow turns change a pad or global length by 10 ms, and consecutive same-target,
same-direction detents within 40 ms change it by 100 ms. Encoder push has no
action, and Return goes directly to the root. Clockwise increases the length
(slower), while counter-clockwise decreases it. Pattern `Cycles` remains a
separate repeat multiplier: it changes pattern wrap length, not this timing
interval. For example, a
106,500 ms Cycle length with pad values 71 and 73 gives a 71:73 polyrhythm
whose 71 side is exactly 40 BPM. The interval uses a saturating `u32`
millisecond value, whose representational limit is about 49.7 days. There is a
hard admission ceiling of eight scheduled voice starts per pad in each
128-frame audio block. Dense grids keep advancing their exact clock, pattern,
and visual pulses when excess audible starts are skipped.

## Mute control

Tap the bottom-left Mute key for less than 300 ms to toggle persistent mute;
hold it for at least 300 ms to mute only until release. The cutoff is defined
by `MUTE_TAP_THRESHOLD_MS`, so it can be tuned easily. Mute targets the selected
beat, or the entire sequencer when no beat is selected. The target is captured
when Mute is pressed and does not change if the selection changes during that
gesture.

Mute has the same behavior on every page, including Pattern. The encoder button
is the only control that toggles highlighted Pattern triggers or opens and
confirms the `All` choice.

Muting suppresses triggers rather than merely silencing their sample output.
Active voices fade out over 32 audio frames (about 1.45 ms), avoiding the click
of an abrupt cut. The clock, pattern position, and visual beat pulses continue
advancing in the background, so unmuting resumes on the next scheduled enabled
trigger without resetting phase.

The Mute key is red at the configured LED brightness. It is bright when the
displayed target is unmuted and dimmed to 20% of that brightness when muted.
With no beat selected it displays global mute; with a beat selected it displays
only that beat's own mute state, even if global mute is active.

## Volume control

Hold the bottom-middle Volume key and turn the encoder to adjust volume from
0 through 100%. With no beat selected, this changes the global master volume;
with a beat selected, it normally changes that pad's volume. Pattern mode is
the exception: with a selected beat, Volume adjusts the highlighted trigger or
all stored trigger levels as described above. The target follows the persistent
selection and Pattern cursor dynamically while Volume remains held. Releasing
Volume returns encoder control to the open menu or mode.

Master and per-beat volumes are independent linear amplitude percentages,
initially 100%. A beat's effective gain is its own percentage multiplied by
the master percentage; changing the master never overwrites per-beat settings.
Slow encoder turns change the selected volume by 1%. Consecutive detents on the
same target and in the same direction within 40 ms change it by 10%. While
Volume is held, the OLED displays `Master N%` or `P# Vol N%` for the live
target.

A scheduled voice captures the Pattern row's trigger level when it starts, so
later edits affect future hits without changing an existing sample tail. The
captured trigger gain multiplies the owning pad's live ramp before voices are
summed; the live master ramp multiplies that sum. In short, the gain order is
trigger × pad × master, with saturation only after mixing.

The Volume key is yellow, with its intensity showing the live target's stored
percentage rather than the combined effective gain. Its output is also scaled
by the configured key brightness, and it turns fully off at 0% volume. Mute,
Volume, and Return retain their red, yellow, and white identities during the
Light-mode preview.

Volume targets are sampled at audio-buffer boundaries. Master and every pad
then follow independent 64-frame linear ramps (about 2.90 ms), so changing one
does not reset another ramp. At 0%, voices can still start and advance silently
when capacity is available, so raising the volume can reveal the remaining
tail of a sample. A pattern trigger whose own stored level is 0% still advances
the clock and produces its visual pulse, but it skips voice allocation because
its captured gain could never become audible. Mute takes precedence: it
continues to suppress new triggers and fades active voices over 32 frames.

## Sample mode

Without a selected beat, Sample displays `Select voice`. With a beat selected,
turn the encoder to choose its sound from one flat list of 24 samples. Each
detent moves exactly one entry, clamps at the first and last sample, and never
accelerates.
Changing the selected beat displays its current assignment without previewing
it. Encoder movement changes the assignment and normally requests a preview;
hold the physical encoder button while turning to browse and assign samples
silently. The hold affects only preview generation—the sample selection still
moves normally and remains clamped at the catalog endpoints.

Sample changes use one latest-wins preview mailbox. If several previewing
detents arrive before the audio task's next control snapshot, only the latest
pending preview is started. A request captured by that snapshot starts at the
next buffer boundary; one arriving just after it waits one additional boundary,
so the safe double-buffered path has less than two buffers (about 11.61 ms) of
worst-case latency. Preview obeys the pad's mute, pad volume, and master volume,
uses a full 100% trigger gain rather than a Pattern slot's level, and pulses
that pad's NeoPixel. It does not advance the sequencer clock or pattern.
Scheduled audio has priority if the voice pool is busy, so a preview may be
skipped without affecting the selected sample. Preview is also the first
feature disabled temporarily when the measured audio workload enters pressure
mode.

The OLED shows the sample's short name while browsing.

## Light mode and beat LEDs

Light mode adjusts NeoPixel brightness from 0 through 100%; brightness starts
at 50%. Slow turns change it by 1%, and consecutive detents in the same
direction within 40 ms change it by 10%. Light mode supplies a steady full
palette as the base state for all nine beat keys.

Outside Light mode, an idle beat key's base state is off. The selected beat is
shown as steady white at the configured brightness. For an unselected key, a
scheduled trigger or automatic preview shows its normal palette color for 100
ms. On the selected key, that palette contributes 20% to the white selection
color, so even continuous fast triggers remain predominantly white. Light mode
supplies palette colors, with the same selected-key tint on triggers.

After that base color is computed, a selection applies an additional 80%
dimming layer to every other beat key: off remains off, while another pad's
trigger, preview, or Light-mode color is multiplied to 20% of what it otherwise
would have shown. With no selection, no dimming layer is applied. The bottom
controls are excluded: Mute remains red, Volume yellow, and Return white.

## Saving and loading songs

LoopTic has 256 fixed song slots named by a three-digit number and a stable
animal name, from `001 Aardvark` through `256 Zebu`. Names are firmware-owned
rather than editable song data. Nothing is loaded automatically at boot: the
initial state is shown as `Unsaved`, and Save, Load, Copy, and Delete are all
explicit operations.

The root `Save` entry saves immediately back to the current slot. If the live
song has not changed since its last Save or Load, it reports `No changes` and
does not write flash. If there is no current slot, Save opens the slot browser
in Save-as mode. The root header shows the current three-digit slot or
`Unsaved`; `*` means live musical state differs from the last saved or loaded
revision.

`Songs` opens `Load`, `Save as`, `Copy`, and `Delete`. Selecting an operation
opens a bounded slot browser; `*` marks a stored slot and `-` an empty one.
Slow turns move one slot and eligible fast turns move ten, always clamping at
`001` and `256` rather than wrapping. Each operation uses a `Cancel`-first
confirmation. Copy reads one stored slot and writes another without changing
the live song. Copy intentionally transfers the raw, self-versioned record: it
can preserve an unsupported record for use by another firmware version, but it
can also duplicate a corrupt record; only Load decodes and validates musical
state. Copy and Save-as confirmations warn before replacing an occupied
destination, and Load warns before discarding dirty live edits. Deleting the
current slot leaves the live music intact but changes its identity to
`Unsaved`. Return cancels any staged choice. Once the OLED says an operation is
in progress, flash programming cannot be cancelled safely and its Busy screen
cannot be dismissed; Return records the underlying navigation request while
completion continues. Completed and error screens remain visible until Return
or an encoder press acknowledges them.

A song stores the global Cycle length; all nine Beats values, optional per-pad
Cycle-length overrides, Pattern Cycles multipliers, sample assignments,
patterns, and trigger levels; latched global/per-pad mute; and master/per-pad
volume. It deliberately excludes active sample tails, playback phase, UI
cursors and selection, momentary mute, brightness, overload state, and
diagnostics. Confirmed Reset all clears the current-slot association so a later
root Save cannot accidentally overwrite the previously loaded song.

The final 2 MiB of flash is a linker-reserved journal. A CRC-protected
superblock versions the physical layout, and every Postcard song record has a
separate schema version. Firmware reports unsupported layouts or records and
leaves them untouched instead of treating them as empty. Flash operations
first fade active voices and stop PIO/DMA, then re-prime audio afterward;
musical time pauses during the explicit operation. See the
[song storage architecture](docs/song-storage.md) for the exact partition map,
compatibility rules, and power-loss model.

New saves use song format v3. Loading a v2 song preserves all of its musical
state and assigns every pad to the global Cycle length, matching v2 behavior.
Loading alone does not rewrite the stored v2 record. The next operation that
actually writes the live song—Save after an edit or Save-as—encodes v3; raw Copy
preserves the source's original version. V1 records remain unsupported.

The first confirmed Save erases and initializes the complete reserved song
partition before writing slot data, so it can take noticeably longer than a
later Save. A blank partition is never initialized merely by booting.

## Reset all

Selecting `Reset all` opens `Cancel` and `Reset`, with `Cancel` selected by
default. Each encoder detent moves one choice without acceleration, and the
choice clamps rather than wrapping at either end. Pressing the encoder exits the
confirmation. `Cancel` returns to the root without changing musical settings;
confirming `Reset` restores the 1000 ms global Cycle length, clears every
per-pad Cycle-length override, sets all Beats values to 0 and Pattern Cycles to
1x, fills every pattern enable map, restores every trigger level to 100%,
restores the default AKU kick mapping on pads 1–6 and AKU open-hat mapping on
pads 7–9, clears global and per-pad mute, sets master and per-pad volume to
100%, and clears pending previews and visual pulses.

At the next audio-block boundary, every active primary voice begins the normal
32-frame release rather than stopping abruptly. Existing forced-fade tails are
neither restarted nor cleared; they finish their already bounded fades.

Reset does not change LED brightness, rewind the playback clock, or clear load
control and diagnostic history. It returns to the root with `Reset all` still
highlighted, no beat selected, and no current song-slot association. Neither
`Cancel` nor Return changes musical settings or the current song. Cancel or
Return from the confirmation preserves the current selection; only a confirmed
Reset or a Return pressed at the root clears it.

## Voice behavior and overload protection

Scheduled hits use 24 primary slots without any per-pad partition, so
retriggering a pad does not normally cut off its previous sample and different
pads remain independent. Separate pads that select the same WAV still start
separate voices on the same frame. Only multiple scheduled ticks from the
**same pad** that land on one exact audio frame coalesce into one hit. If those
ticks carry different trigger levels, the coalesced hit uses the loudest one;
the non-contiguous recovery path follows the same rule for overdue ticks.

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

The menu UI no longer has an OLED diagnostics screen. The firmware retains
high-water marks and cumulative counters for load transitions, the minimum
effective voice limit, coarse-dither frames, skipped
previews and triggers, discarded fade tails/primaries, clipping, steals, and
voice peaks, together with renderer/service timing, DMA cadence, deadline misses,
and PIO underruns. They remain in memory but the current firmware does not
stream them over RTT; inspect them with a debugger or add explicit logging to a
benchmark build. GP13 still latches only for an actual underrun or fatal
hardware or initialization error; entering pressure mode by itself is not a
fault.

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
| Mute / Volume / Return | GP10 / GP11 / GP12, active low | GPIO inputs with pull-ups |
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

Run the formatting plus host and firmware Clippy checks used before flashing:

```console
cargo fmt --check
cargo clippy --lib --target x86_64-unknown-linux-gnu --locked -- -D warnings
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
The linker caps firmware at the first 6 MiB, so the generated UF2 contains no
blocks in the final 2 MiB song partition; ordinary `cargo flash` firmware
updates preserve saved songs.

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

Verify that both the ELF load images and every UF2 block remain below the song
partition boundary:

```console
scripts/check-firmware-layout.sh
```

Copy `looptic.uf2` to the mounted `RPI-RP2` volume. This is equivalent to the
automated `cargo flash` workflow.

## Install or debug with probe-rs

Connect an SWD probe to the MacroPad debug pads and run:

```console
cargo firmware
probe-rs run --chip RP2040 target/thumbv6m-none-eabi/release/looptic
```

This flashes the ELF and attaches to the `defmt` RTT channel. The retained
audio counters are not streamed there by default. A probe is optional and is
not needed for USB deployment. Check the probe tool's erase policy before using
it with saved songs: programming the ELF by sector can preserve the reserved
tail, but an explicit full-chip/mass erase always destroys the song partition.

## Hardware verification

The one-handed UI, OLED, LED, Return/Reset, and uninterrupted-audio checks below
remain pending because no MacroPad was connected while implementing this UI.

After flashing, verify all nine beat keys, both encoder directions, the
complete OLED image, each beat NeoPixel color and brightness range, and the
initial kick/hat split across pads 0–5 and 6–8. Confirm that the bottom row is
Mute, Volume, and Return from left to right; none should create sequencer beats.
Mute must remain red, Volume yellow, and Return white.

Verify the root order `Beats`, `Cycle length`, `Pattern`, `Sample`, `Light`,
`Save`, `Songs`, `Reset all`, the initial Beats highlight, five-row scrolling,
one-detent root movement without acceleration, clamping at both ends,
encoder-button entry, and the remembered root cursor. Select a beat with
one press, release it, and confirm that selection persists; press it again to
clear, and press a different beat to replace it.
Repeat selection changes at the root and in every mode. Return from each mode
and confirmation must restore the root while preserving selection; a second
Return at the root must clear it. Return must cancel an uncommitted choice, take
priority over simultaneous key-press and encoder-button press edges, and wait
for already-held controls to be released before accepting them again.

With blank storage, verify root Save opens Save as and initializes only after
confirmation. Save several distinct songs, reboot, and confirm that the slot
markers survive while no song auto-loads. Load each song, edit it, check the
root dirty `*`, then use root Save and verify a second unchanged Save performs
no write. Exercise Copy and Delete (including the current slot), every
Cancel-first confirmation, empty-slot feedback, and Return from each staged
screen. Reflash with `cargo flash` and verify all stored songs remain. During
power-cut and incompatible-format tests, unsupported layouts and records must
be reported and left intact rather than reformatted silently.

Enter Pattern with no selection and check `Select voice`. Select pads and
verify their independent remembered cursors. At division 8, edit slots in both
groups of four, reduce the division to 4, and confirm that later edits are
hidden rather than erased; returning to 8 must reveal them. Check that division
0 shows `Cycles` and `All` with no trigger rows. Confirm Pattern scrolling
clamps at `Cycles` and the final visible trigger, including accelerated
overshoot. Open the `All` choice on
full, mixed, and empty maps: it must default to `Cancel` and clamp at `Cancel`
and `None`; `All` must fill the complete 256-bit map, and `None` must clear it,
including hidden slots. Committing either `All` or `None` must reset all 256
stored trigger levels to 100%; `Cancel` and Return must preserve both the
complete map and every stored level. Verify that only the encoder button
toggles an individual row or opens and confirms the `All` choice.

In Beats, verify that an empty selection shows `Select voice` and ignores
encoder turns. Select each pad and verify direct 0–256 Beats editing with the
documented slow and accelerated steps. Encoder push must do nothing, Return must
go directly to the root, and changing or clearing selection must retarget the
screen without leaving Beats.

In Cycle length with no selection, verify direct editing of the global value
down to its 50 ms minimum. With a pad selected, verify direct per-pad editing
with the documented slow and accelerated steps. Confirm counter-clockwise from
50 ms selects `Length 0 (Global)`, values 1 through 49 never appear, and
clockwise from 0 selects 50 ms. Change the global length while a pad is at 0 and
verify that pad follows it; give the pad a nonzero length and verify it retains
its own cadence across later global edits. Encoder push must do nothing, Return
must go directly to the root, and all pads must retain independent settings.

Test short Mute taps and 300 ms holds both globally and with a beat selected;
verify that a selection made before the gesture targets that pad and that the
gesture keeps its captured target if selection changes. Confirm
the Mute LED is bright red when its displayed target is unmuted, is dimmed to
20% when muted, and shows only the selected beat's local setting while a beat
is selected. Muted voices should fade cleanly in about 1.45 ms while their visual
pulses and phase continue, then resume at the next scheduled trigger after
unmuting. Repeat on a selected beat's Pattern page and verify that short taps
toggle that pad's mute while long holds mute it only until release. Clear the
selection there and confirm that Mute targets the global latch.

Hold Volume and dynamically select, replace, and clear beat targets; verify
fallback to master, 1%/10% adjustment,
OLED `Master N%` and `P# Vol N%` feedback, and the full 0–100% gain range.
Confirm the yellow indicator follows the selected stored setting and
configured LED brightness, turns off at 0%, and does not replace the red or
white indicators. Verify master and per-beat gains multiply, independent
64-frame ramps remove abrupt gain steps, voices can continue silently at zero,
and Mute still suppresses triggers.

In Pattern, verify every visible row shows its enable state and trigger level.
Hold Volume on individual rows and check 1%/10% adjustment, 0–100% clamping,
and `P# T### N%` feedback. Give several visible and hidden slots different
levels, then hold Volume on `All`: every one of the 256 slots must receive the
same relative delta, retain its accent difference until independently clamped,
and contribute to the displayed rounded average. Disabled slots must retain
and accept level edits. Confirm a 0% enabled trigger still pulses visually but
does not consume a voice, changing a level affects only future voices, and the
yellow indicator follows the selected trigger or whole-map average.

Enter Sample with no beat selected and check `Select voice`. Select each beat
and browse all 24 names with one entry per detent and no acceleration. Confirm
the selection clamps at the first and last samples in both directions, without
replaying an unchanged endpoint. Switching beats must display their current
assignments without previewing. Turning normally must change the assignment and
preview within the documented two-buffer worst case. Hold the physical encoder
button while turning and verify that assignments still move but remain clamped
and previews are suppressed; release it and previewing must resume immediately.
Rapidly cross several entries and confirm only the latest pending preview
starts. Preview must follow mute and both gains, pulse the target pad, and leave
its pattern and clock unchanged.

In Light, verify 0–100% adjustment with 1%/10% steps. With no selection, all
nine beat palettes should show at the configured brightness. Select one pad:
it must become steady white while the other eight Light colors are multiplied
to 20%. Outside Light, idle pads must remain off rather than becoming a steady
20%. Trigger or preview the selected pad and verify its normal palette color
contributes 20% to the white selection color. Check the same 100 ms indication
at full brightness
with no selection and at 20% on a non-selected pad while another pad is
selected. Mute, Volume, and Return must not be dimmed by beat selection.

Open Reset all and verify the default `Cancel`, one-choice-per-detent movement,
clamping at both ends, and no acceleration. Confirming `Cancel` must return to
the root without changing musical settings. After altering every resettable
category, create active primaries and forced-fade tails, then confirm Reset.
Check global Cycle length 1000 ms, all per-pad Cycle overrides off, all Beats
values 0, Pattern Cycles 1x, every pattern bit on, default AKU mappings, every
trigger level 100%, mute off, and master/per-pad volumes 100%. Pending
previews and visual pulses must clear; active primaries must release over 32
frames while existing fade tails finish without being restarted or cleared.
Brightness, playback position,
load-control state, and diagnostic counters must remain unchanged; the UI must
return to the root with Reset all highlighted, no beat selected, and no current
song association.

Create overlapping tails and verify repeated hits do not normally cut each
other off. Two pads using the same sample must still start separate voices on
the same frame, while only multiple ticks from one pad on one exact frame
coalesce. Verify open and closed hats overlap because no choke groups are
enabled. Drive more than 24 overlapping starts to exercise deterministic
stealing and its 32-frame forced fades without confusing pool pressure with an
audio underrun.

Exercise all beat pads while browsing samples, turning the encoder, updating
patterns, muting, and changing volume for 10 minutes. Inspect the retained
timing and load counters with a debugger, or use a benchmark build that adds
explicit RTT logging. Require zero 5,805 µs audio-service deadline misses, zero
PIO underruns, and GP13 remaining off; a latched red LED means initialization
or real-time audio failed. The preferred settled maximum service time is at or
below 4,350 µs, with load shedding visible rather than accumulating UI work.
A deliberately forced fade-tail overflow is separately diagnostic and may
shorten an existing fade. At the densest supported 50 ms/256 grid, the
per-block start limit is expected and is not an underrun. Separately exercise a
non-contiguous recovery in an instrumented build and confirm that overdue
same-pad ticks coalesce at their maximum stored trigger level.

Reproduce the original overload case separately: disable pads 2–9, select AKU
Kick for pad 1, set a 1,000 ms base interval, and sweep its division through 28
and beyond. The UI must remain responsive, audio may degrade according to the
documented load ladder, and the deadline/underrun counters must remain zero.
This overload acceptance check is also pending. For timing accuracy, leave a
`Beat 1` pattern enabled, record it for 60 seconds, and check that it remains
within 0.02% of one trigger per second.

All 24 WAV files are embedded in XIP flash, so no filesystem copy step is
required at runtime.

## Licensing

LoopTic is licensed under the MIT License. The PWM audio encoder and PIO logic
are derived from Raspberry Pi's BSD-3-Clause-licensed `pico-extras`; see
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
