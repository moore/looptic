# LoopTic user guide

This guide describes the current LoopTic firmware from a player's point of
view. It starts with the musical model and common controls, then walks through
every screen and the complete Tracks and song-storage flows.

## The musical model

LoopTic has nine voices, one for each numbered key. A voice chooses a sample
and schedules hits using four related layers:

| Term | Meaning |
| --- | --- |
| **Cycle length** | The time span over which a voice's Beats are distributed. There is one global value and an optional override for each voice. |
| **Beats** | How many equally spaced ticks a voice receives in one Cycle. Zero disables its scheduled ticks. |
| **Pattern** | Up to 256 slots that enable, disable, and accent individual ticks. |
| **Pattern Cycles** | How many groups of `Beats` slots the Pattern visits before wrapping. This changes Pattern length, not tick speed. |
| **Tracks** | A finite song timeline that allows or suppresses each voice's enabled Pattern hits. |

For example, a voice with 3 Beats and 2 Pattern Cycles still ticks three times
per Cycle, but its Pattern contains six independently editable slots and wraps
after two Cycles.

Tracks is a gate over the Pattern rather than a second sequencer. A disabled
Track span prevents future hits from starting, but Pattern phase keeps moving
and an already-playing sample tail is not cut off. This makes it possible to
arrange sections without changing the underlying rhythm.

The Song length gives that arrangement a finite end. LoopTic can either loop
back to the beginning or stop there.

## Controls at a glance

| Control | Normal behavior | Tracks behavior |
| --- | --- | --- |
| Voice keys 1–9 | Select voices for editing. | Paint Track spans while not playing; live-audition voices while playing. |
| Encoder turn | Move a menu cursor or edit the current value. | Move the time cursor while not playing, change master volume while playing, or zoom while the encoder is held. |
| Encoder push | Enter, toggle, or confirm. | Click to open End behavior; hold and turn to zoom. |
| Mute | Tap for latched mute; hold for momentary mute. | Play/Pause. The key is blue. |
| Volume | Hold and turn the encoder to edit volume. | No modifier is needed: turn the encoder while playing to edit master volume. |
| Return | Close the current editor or return toward the root. | Close End behavior or leave Tracks. |

Voice keys select and edit voices; tapping one does not immediately play its
sample. Sample browsing can request a preview, and Tracks live audition opens
future scheduled hits, but neither turns the key grid into one-shot drum pads.

Menu rows are highlighted as black text on a white band. A triangle at the
left of the first or last visible row means more entries exist off-screen; it
does not consume a menu row. Lists clamp at their endpoints rather than
wrapping.

Clockwise generally moves down, increases a value, or moves later in time;
counter-clockwise moves up, decreases, or moves earlier.

The root header shows `Unsaved` or the current three-digit song slot. An `*`
means the live song differs from its saved or loaded version. A selection is
shown as `P1` for one voice or, for example, `P2+3` for primary voice 2 plus
three additional voices.

## Make a first rhythm

At boot, the transport is running, the global Cycle length is one second, and
every voice has zero Beats. The instrument is therefore silent until at least
one voice receives Beats.

1. At the root, tap voice key 1.
2. With `Beats` highlighted, push the encoder.
3. Turn clockwise to set `Beats 4`.
4. Press Return to return to the root.
5. Open `Sample` and turn the encoder if you want a different sound.
6. Open `Pattern` to disable hits or add per-hit accents.

The default Pattern is fully enabled, so setting Beats is enough to hear the
selected voice. Connect the SynthPlug to a powered line-level input; the
MacroPad's built-in speaker is intentionally disabled.

## Voice selection and group editing

Selection persists while moving between screens. Selected voice keys are
white; unselected voice activity is dimmed while a selection exists. Tracks
hides this selection without clearing it because its voice keys have a
different role.

Selection begins in single-voice mode:

- Tapping an unselected voice makes it the only selection.
- Tapping the selected voice clears the selection.
- Holding two or more voice keys together replaces the selection with that
  exact chord and enters multi-select mode. This works at the root and in any
  screen except Tracks.

In multi-select mode, individual taps add or remove voices. Selection order is
remembered: the earliest selected voice is the **primary**. If it is removed,
the next-oldest member becomes primary. When only one voice remains, the next
single-key action uses the normal exclusive behavior again. A new chord always
replaces the entire group, in ascending key order.

Beats, per-voice Cycle length, Sample, and per-voice Volume support group
editing. Their screens display the primary voice's value. If all selected
voices already match, the first encoder turn edits the whole group. If they do
not match:

1. The first turn changes nothing and opens a mixed-value warning.
2. Further turns are ignored.
3. Push the encoder to copy the displayed primary value to every selected
   voice. The discarded turn is not replayed.
4. Turn again after synchronization to make the intended edit.

Return cancels only the warning. Changing selection, leaving the context,
loading, or resetting also cancels it. A Volume warning remains available for
push confirmation after the Volume key is released.

Pattern editing requires exactly one selected voice. With no selection, the
screen opens and asks for a voice. Trying to enter it with a group shows
`Pattern needs 1 voice` and `Deselect extras` at the root. Forming a chord while
Pattern is open also returns to that warning.

## Return and encoder acceleration

Return normally returns to the root and preserves the complete selection. At
the root, Return clears the selection instead. It first closes these nested
editors without leaving their parent screen:

- a mixed-value warning;
- the Pattern Cycles editor;
- a Song length or Cycle length editor; and
- Tracks End behavior.

Return cancels an uncommitted Pattern All choice or Songs operation. A control
that is still physically held after Return is ignored until released, so its
action cannot leak into the next screen.

Values that support acceleration use a larger step for fast consecutive turns
in the same direction. Changing direction or target restores the fine step.
Cursor movement, Pattern Cycles, Song length, Sample selection, Tracks
navigation/zoom, and confirmation choices remain one step per detent.

## Mute

Outside Tracks, Mute targets the selected voice group, or the global sequencer
when no voice is selected. The target is captured when the key is pressed and
does not change during the gesture.

- Release before 300 ms to toggle a persistent mute latch.
- Hold for at least 300 ms to mute only for the duration of the hold.

A group tap sets every selected voice to the opposite of the primary voice's
latched state. A group hold momentarily mutes the captured members and restores
their individual latched states on release.

Mute suppresses future starts and releases active voices over a short fade.
Clock and Pattern phase continue, so unmuting resumes on the next scheduled
enabled hit. The Mute key is red: bright for an unmuted target and dim for a
muted target.

Tracks replaces all of this with Play/Pause; see [Tracks](#tracks).

## Volume

Outside Tracks, hold Volume and turn the encoder. With no selection this edits
master volume. With a selection it edits the selected voice or group. Master
and voice volumes are independent 0–100% values and are both saved with the
song.

Pattern changes the target according to its highlighted row:

- a trigger row edits that slot's trigger level;
- `All` applies the same relative change to all 256 stored trigger levels; and
- `Cycles` falls back to the selected voice's ordinary volume. With no selected
  voice or Pattern row, Volume targets master instead.

Slow turns use 1% steps and fast turns use 10% steps. Audio gain changes are
ramped to avoid clicks. A trigger captures its own level when it starts, so
editing an accent affects future hits without changing an existing tail.

The Volume key is yellow, with intensity proportional to the current target's
stored value. In Tracks, the key is not a modifier; ordinary encoder turns
adjust master volume only while playing.

## Root menu and screens

The root menu is ordered:

1. Beats
2. Song settings
3. Pattern
4. Tracks
5. Sample
6. Light
7. Save
8. Songs
9. Reset all

The root cursor is remembered when a screen closes.

## Beats

Beats sets how many ticks each selected voice receives during its effective
Cycle length.

- Range: 0–256.
- Slow step: 1.
- Fast step: 10.
- No selection: the screen shows `Select voice` and turns do nothing.

Beats controls cadence; Pattern Cycles controls only how many slots are visited
before the Pattern wraps. If a Beats increase leaves too little room for the
current Pattern Cycles within 256 slots, Cycles is clamped independently for
each affected voice. Hidden Pattern enable and trigger-level data is preserved.

## Song settings

Song settings contains `Song length` and `Cycle length`. Turn to choose a row
and push to edit it. Push or Return closes an editor and returns to the Song
settings list; a second Return returns to the root.

### Song length

Song length defines the exclusive end of the Tracks arrangement.

- Range: `00:01` through `99:59`.
- Default: `03:00`.
- Step: one second per detent, without acceleration.

Shortening a song does not delete Track changes beyond the new end. They
reappear if the song is lengthened again. If the new end falls behind the live
playhead, the selected Loop or Stop behavior is applied at the next audio
boundary.

### Cycle length

With no voice selected, this editor changes the global Cycle length. With a
selection, it changes each selected voice's stored override.

- Global and independent minimum: 50 ms.
- Slow step: 10 ms.
- Fast step: 100 ms.
- Per-voice value `0`: follow Global.

Turning clockwise from a per-voice `0` enters at 50 ms. Turning below 50 ms
returns directly to `0`; values 1–49 are skipped. An explicit override equal to
the current global length is still independent and is treated as different
from `0` during mixed-value checks.

## Pattern

With no selected voice, Pattern shows `Select voice`. With one selected voice,
its rows are:

1. `Cycles N×`
2. `All`
3. one row for every active trigger slot

The active slot count is `Beats × Cycles`, capped at 256. A three-Beat voice at
2× therefore exposes six slots. Each trigger row shows `ON` or `off` and its
0–100% trigger level. Turning moves the cursor; push toggles the highlighted
trigger.

Every voice permanently retains all 256 enable bits and trigger levels. A
shorter active Pattern merely hides the suffix, so expanding Beats or Cycles
reveals the previous data.

### Cycles

Push `Cycles` to open its editor. The allowed range is 1 through
`floor(256 / Beats)`; a zero-Beat voice remains at 1×. Turns change one step at
a time without acceleration. Push or Return to return to the Pattern list.

Changing Cycles realigns the Pattern position to the global tick ordinal. It
does not change tick cadence or the next timing deadline.

### All

Push `All` to open `Cancel`, `All`, and `None`, with Cancel selected initially.

- `All` enables every one of the 256 slots.
- `None` disables every slot.
- Either committed choice resets all 256 trigger levels to 100%.
- `Cancel` leaves both maps unchanged.

The `All` row summarizes the complete stored map, including hidden slots, and
shows the rounded average of all trigger levels. Holding Volume on this row
applies a relative change to every stored level, so existing accents remain
different until individual values clamp at 0% or 100%.

With zero Beats, Pattern shows `No triggers`, but Cycles and All remain
available.

## Tracks

Tracks arranges the nine underlying Patterns across the finite Song length.
Ordinary voice selection is preserved but hidden while the screen is open.

### Reading the display

The header contains:

- `>` while playing, `|` while paused, or `X` after Stop reaches song end;
- the playhead or cursor as `MM:SS.mmm`;
- `L` for Loop or `S` for Stop; and
- the current zoom label.

Columns 1–9 correspond to the nine voices. A Pattern hit is drawn as a large
dot: filled when its Track is enabled and hollow when disabled. A thin vertical
line runs through enabled spans. If several hits share one display row, they
are coalesced and an enabled hit takes visual priority. The horizontal line is
the playhead while playing and the edit cursor otherwise.

An underline beneath a column number marks a voice currently being painted or
live-auditioned.

### Play, pause, Loop, and Stop

In Tracks, Mute becomes a blue Play/Pause key. It is bright while playing and
dim while paused or stopped.

- Press while playing to pause. Song time and every Pattern phase freeze;
  active sounds fade briefly.
- Press while paused to resume from the cursor's next unrendered frame.
- Press after Stop has reached song end to restart at `00:00`.
- Starting at a cursor includes a projected trigger exactly at that position.

Click the encoder without turning to open End behavior. Choose `Loop` or
`Stop`, then click or Return to close the overlay. Loop fades active voices,
resets Pattern phases, and continues immediately at `00:00`. Stop fades active
voices and remains at song end.

End behavior is runtime-only: Load preserves the current choice, while reboot
and Reset all restore Loop.

### Move and zoom

While not playing, each ordinary encoder detent moves to the previous or next
projected enabled Pattern trigger across all voices. Track gates and Mute do not
remove these projection boundaries. Moving before the first or after the last
projected hit reaches `00:00` or song end. Movement does not accelerate.

Hold the encoder and turn to zoom around the cursor or playhead. The available
view durations are:

`50 ms`, `100 ms`, `250 ms`, `500 ms`, `1 s`, `2 s`, `5 s`, `10 s`, `30 s`,
`1 m`, `2 m`, `5 m`, `10 m`, `20 m`, `1 h`, and the whole song.

The default is 10 seconds. Zoom changes only the display and is not saved.

### Paint Track spans

Pause first, then hold one or more voice keys at the desired anchor. Turn the
encoder to move the other boundary and release all captured keys to commit the
half-open interval. Painting works forward or backward.

Each voice toggles relative to its own state at the anchor. This means one
chord can enable some voices and disable others while preserving their
different starting states. A change at a boundary applies before a hit at that
same time; the hit at the ending boundary is excluded.

If no encoder turn occurs, tapping a voice paints from the cursor through the
next projected trigger boundary, or through song end when no later trigger
exists. The cursor moves to the end of the committed edit.

The complete chord edit is atomic. The timeline can store 256 canonical change
points; if the edit cannot fit, LoopTic changes nothing and shows
`Tracks full / Simplify spans`. Merge adjacent spans or paint a larger uniform
region before retrying.

### Live audition

While Tracks is playing, hold one or more voice keys to make those voices'
future scheduled Pattern hits audible over the arrangement. Live audition:

- bypasses both Track gates and ordinary global/voice mute;
- does not fire a hit immediately;
- does not alter or dirty the song; and
- stops affecting future hits on release, while existing sample tails finish.

Ordinary encoder turns adjust persistent master volume while playing and show
brief volume feedback. Pause before moving the cursor or painting.

Leaving Tracks clears live audition but does not pause transport; playback
continues on whichever screen opens next.

Track boundaries remain at absolute song times when Beats, Cycle length,
Pattern Cycles, or Pattern contents change. The display projection and
scheduler realign to the new musical timing.

## Sample

Sample assigns one of 24 embedded sounds to the selected voice or group.

- No selection: `Select voice`.
- Step: one sample per detent, without acceleration.
- Endpoints clamp rather than wrap.

An assignment normally previews once through the primary voice. Hold the
encoder button while turning to assign samples silently. Changing selection
alone does not preview. Preview obeys the primary voice's Mute and Volume and
master Volume; under heavy audio load it may be skipped without undoing the
assignment.

## Light

Light sets NeoPixel brightness from 0–100%, defaulting to 50%. Slow turns use
1% steps and fast turns use 10% steps. The screen displays the full voice color
palette as a preview.

Outside Light, selected voices are steady white at the configured brightness.
A trigger contributes 20% of its palette color to that white, so very fast
activity remains visibly selected. When any selection exists, unselected voice
activity is additionally dimmed to 20%. Mute, Volume, and Return retain their
red, yellow, and white identities; Tracks changes Mute to blue.

Brightness is runtime-only state and is not stored in songs or changed by
Reset all. Reboot restores the 50% default.

## Save

The root `Save` entry writes immediately to the current slot. If the song has
not changed since its last Save or Load, it returns silently without writing
flash. If there is no current slot, it opens the Save-as browser.

Choosing a Save-as destination writes immediately, even if the slot is already
occupied. There is no Save or overwrite confirmation and no completion dialog;
wait while the Busy screen is shown.

## Songs

Songs contains `Load`, `Save as`, `Copy`, and `Delete`. LoopTic has 256 stable
slots named from `001 Aardvark` through `256 Zebu`. In a browser, `*` marks an
occupied slot and `-` marks an empty slot. Slow turns move one slot and fast
turns move ten.

### Load

Selecting an occupied slot loads immediately when the current song is clean.
If that occupied slot is selected while the current song has unsaved changes,
LoopTic first shows a Cancel-first warning that those changes will be lost. An
empty slot skips the warning and shows an error without changing the live song.
A successful Load returns without a completion dialog, starts the loaded song
at `00:00`, and preserves the runtime End behavior and zoom.

### Save as

Selecting a destination writes immediately with the same no-confirmation rule
as root Save. The live song becomes associated with that slot.

### Copy

Choose a source and destination, then confirm. Copy transfers the raw stored
record without loading it into the live song. It can therefore preserve and
copy a song record from another supported firmware generation, but Load is the
operation that decodes and validates musical data.

### Delete

Choose a slot and confirm. Deleting the current slot leaves the live music
unchanged but changes its identity to Unsaved.

Copy and Delete retain dismissible completion screens. Return cancels a staged
browser or confirmation. Once a Busy or formatting operation begins, it cannot
be cancelled safely.

### Storage initialization and compatibility

Blank storage is initialized only by the first explicit Save. That Save may
take noticeably longer because LoopTic verifies the complete reserved map. An
already-erased map is not needlessly erased; a nonblank or interrupted map is
erased sector by sector. The UI remains responsive while audio is paused.

If the physical storage layout is valid but unsupported, its status screen can
lead to an explicit Cancel-first Format confirmation. Format erases all songs
and reports percentage progress. Corrupt storage does not offer Format because
its identity and bounds cannot be trusted.

Current saves use song-record format v4. V2 and v3 songs load with a default
three-minute, fully enabled Tracks arrangement and are written as v4 after a
later musical edit and Save, or after Save as. V1 and unknown versions are
reported as unsupported rather than rewritten.

Normal LoopTic UF2 firmware updates leave the final 2 MiB song partition alone.
A debugger's full-chip or mass erase destroys it.

## Reset all

Reset all opens `Cancel` and `Reset`, with Cancel selected. The choices move one
step per detent and do not wrap. Confirming Reset restores:

- a one-second global Cycle and no per-voice overrides;
- zero Beats and 1× Pattern Cycles on every voice;
- all 256 Pattern slots enabled and all trigger levels at 100%;
- the default sample assignments (kick on voices 1–6 and open hi-hat on
  voices 7–9);
- a three-minute Song length with every Track enabled;
- global and per-voice Mute off; and
- master and per-voice Volume at 100%.

Active sounds release over a short fade. Reset also starts song transport at
`00:00`, restores Loop and the 10-second Tracks zoom, clears voice selection,
and changes the live song to Unsaved. It does not change LED brightness or
clear internal load-control and diagnostic history.

Cancel and Return preserve the live song and selection. Reset all does not
erase stored song slots.

## What a song stores

Saved musical state includes:

- Song length and the Track timeline;
- global and per-voice Cycle lengths;
- Beats, Pattern Cycles, all Pattern enable bits, and trigger levels;
- sample assignments;
- latched global and per-voice Mute; and
- master and per-voice Volume.

Runtime performance state is not saved: transport position, Loop/Stop, Tracks
zoom and cursor, live audition, active sample tails, selection/order,
mixed-value warnings, momentary mute, LED brightness, overload state, and
diagnostic counters.

## Troubleshooting

### The instrument boots but is silent

All Beats default to zero. Select a voice and set Beats above zero. Also check
that the SynthPlug is connected to a powered line input, master and voice
Volume are above zero, and the relevant global, voice, and Track gates permit
the hit.

### A selected voice does not change with the encoder

The first turn on a mixed group intentionally opens a warning. Push to copy the
primary value to the group, then turn again. Pattern editing instead requires
exactly one selected voice.

### Tracks shows hollow dots

The underlying Pattern contains those projected hits, but that Track is
disabled at those times. Pause and paint the voice span to enable it, or hold
the voice while playing for temporary live audition.

### The first Save is slow

Initial storage verification touches the complete two-megabyte partition and
may take much longer than later writes. Wait for the Busy screen to clear. An
explicit Format additionally shows erase percentage.

### GP13 stays lit

The red status LED latches on a fatal catalog/OLED initialization failure, an
OLED communication failure, or a real PIO audio underrun. Reboot once; if it
returns, verify the firmware image and device hardware. Ordinary adaptive
quality reduction does not light GP13.

For build and installation help, return to the [README](../README.md). For the
implementation behind these behaviors, see [Internals](internals.md).
