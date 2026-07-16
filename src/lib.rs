#![no_std]

//! Platform-independent audio, sequencing, and UI state for LoopTic.
//!
//! The RP2040 firmware lives in `main.rs`. Keeping this module free of HAL
//! dependencies makes the timing and sample conversion code testable on a host.

pub const PAD_COUNT: usize = 12;
pub const SAMPLE_RATE: u32 = 22_050;
pub const AUDIO_BLOCK_FRAMES: usize = 128;
pub const MAX_BEAT_MULTIPLIER: u16 = 2_048;
pub const PATTERN_BITS: usize = 2_048;
pub const PATTERN_BYTES: usize = PATTERN_BITS / 8;
pub const DEFAULT_BASE_INTERVAL_MS: u32 = 1_000;
// Faster-than-audio trigger grids are coalesced to one trigger per sample frame.
pub const MIN_BASE_INTERVAL_MS: u32 = 50;
pub const BASE_INTERVAL_STEP_MS: u32 = 10;
pub const FAST_ENCODER_MULTIPLIER: i32 = 10;
pub const FAST_ENCODER_THRESHOLD_MS: u64 = 75;
pub const DEFAULT_LED_BRIGHTNESS_PERCENT: u8 = 50;

const SAMPLE_COUNT: usize = 2;
const KICK_INDEX: usize = 0;
const OPEN_HAT_INDEX: usize = 1;
const PWM_QUANTIZED_MAX: u32 = 127;
const PWM_FRACTION_MASK: u32 = 0x1ff;
const PWM_COMMAND_BITS: u32 = 14;
const PWM_DITHER_CYCLES: u32 = 16;

/// Centered PWM command used while no PCM data is available.
pub const SILENCE_PWM_WORD: u32 = 64 | (63 << 7);

/// A fixed-resolution pattern spanning one complete base interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pattern {
    bits: [u8; PATTERN_BYTES],
}

impl Pattern {
    pub const fn all_enabled() -> Self {
        Self {
            bits: [u8::MAX; PATTERN_BYTES],
        }
    }

    pub fn bit(&self, index: usize) -> Option<bool> {
        (index < PATTERN_BITS).then(|| self.bits[index / 8] & (1 << (index % 8)) != 0)
    }

    pub fn set_bit(&mut self, index: usize, enabled: bool) -> bool {
        if index >= PATTERN_BITS {
            return false;
        }
        let mask = 1 << (index % 8);
        if enabled {
            self.bits[index / 8] |= mask;
        } else {
            self.bits[index / 8] &= !mask;
        }
        true
    }

    pub fn step_enabled(&self, step: u16, division: u16) -> Option<bool> {
        let (start, _) = pattern_step_range(step, division)?;
        self.bit(start)
    }

    pub fn set_step_enabled(&mut self, step: u16, division: u16, enabled: bool) -> bool {
        let Some((start, end)) = pattern_step_range(step, division) else {
            return false;
        };

        let first_byte = start / 8;
        let last_byte = (end - 1) / 8;
        let first_mask = u8::MAX << (start % 8);
        let last_width = (end - 1) % 8 + 1;
        let last_mask = u8::MAX >> (8 - last_width);

        if first_byte == last_byte {
            set_masked_bits(&mut self.bits[first_byte], first_mask & last_mask, enabled);
            return true;
        }

        set_masked_bits(&mut self.bits[first_byte], first_mask, enabled);
        self.bits[first_byte + 1..last_byte].fill(if enabled { u8::MAX } else { 0 });
        set_masked_bits(&mut self.bits[last_byte], last_mask, enabled);
        true
    }

    pub fn fill(&mut self, enabled: bool) {
        self.bits.fill(if enabled { u8::MAX } else { 0 });
    }
}

fn set_masked_bits(byte: &mut u8, mask: u8, enabled: bool) {
    if enabled {
        *byte |= mask;
    } else {
        *byte &= !mask;
    }
}

impl Pattern {
    pub fn toggle_step(&mut self, step: u16, division: u16) -> Option<bool> {
        let enabled = !self.step_enabled(step, division)?;
        self.set_step_enabled(step, division, enabled);
        Some(enabled)
    }
}

impl Default for Pattern {
    fn default() -> Self {
        Self::all_enabled()
    }
}

pub fn pattern_step_range(step: u16, division: u16) -> Option<(usize, usize)> {
    if division == 0 || division > MAX_BEAT_MULTIPLIER || step >= division {
        return None;
    }
    let division = usize::from(division);
    let step = usize::from(step);
    let start = step * PATTERN_BITS / division;
    let end = (step + 1) * PATTERN_BITS / division;
    Some((start, end))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WavError {
    TooShort,
    InvalidRiff,
    TruncatedRiff,
    TruncatedChunk,
    MissingFormat,
    MissingData,
    UnsupportedFormat,
    OddDataLength,
}

/// A validated mono, signed 16-bit, 22.05 kHz PCM WAV sample.
#[derive(Clone, Copy, Debug)]
pub struct WavPcm16<'a> {
    data: &'a [u8],
}

impl<'a> WavPcm16<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, WavError> {
        if bytes.len() < 12 {
            return Err(WavError::TooShort);
        }
        if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err(WavError::InvalidRiff);
        }

        let riff_size = read_u32(bytes, 4).ok_or(WavError::TooShort)? as usize;
        let riff_end = riff_size.checked_add(8).ok_or(WavError::TruncatedRiff)?;
        if riff_end > bytes.len() || riff_end < 12 {
            return Err(WavError::TruncatedRiff);
        }

        let mut offset = 12_usize;
        let mut format_seen = false;
        let mut data = None;

        while offset < riff_end {
            if offset.checked_add(8).is_none_or(|end| end > riff_end) {
                return Err(WavError::TruncatedChunk);
            }

            let chunk_id = &bytes[offset..offset + 4];
            let chunk_len = read_u32(bytes, offset + 4).ok_or(WavError::TruncatedChunk)? as usize;
            let chunk_start = offset + 8;
            let chunk_end = chunk_start
                .checked_add(chunk_len)
                .ok_or(WavError::TruncatedChunk)?;
            if chunk_end > riff_end {
                return Err(WavError::TruncatedChunk);
            }

            match chunk_id {
                b"fmt " => {
                    if chunk_len < 16 {
                        return Err(WavError::UnsupportedFormat);
                    }
                    let audio_format =
                        read_u16(bytes, chunk_start).ok_or(WavError::TruncatedChunk)?;
                    let channels =
                        read_u16(bytes, chunk_start + 2).ok_or(WavError::TruncatedChunk)?;
                    let sample_rate =
                        read_u32(bytes, chunk_start + 4).ok_or(WavError::TruncatedChunk)?;
                    let block_align =
                        read_u16(bytes, chunk_start + 12).ok_or(WavError::TruncatedChunk)?;
                    let bits_per_sample =
                        read_u16(bytes, chunk_start + 14).ok_or(WavError::TruncatedChunk)?;
                    if audio_format != 1
                        || channels != 1
                        || sample_rate != SAMPLE_RATE
                        || block_align != 2
                        || bits_per_sample != 16
                    {
                        return Err(WavError::UnsupportedFormat);
                    }
                    format_seen = true;
                }
                b"data" => {
                    if chunk_len & 1 != 0 {
                        return Err(WavError::OddDataLength);
                    }
                    if data.is_none() {
                        data = Some(&bytes[chunk_start..chunk_end]);
                    }
                }
                _ => {}
            }

            let padded_len = chunk_len
                .checked_add(chunk_len & 1)
                .ok_or(WavError::TruncatedChunk)?;
            offset = chunk_start
                .checked_add(padded_len)
                .ok_or(WavError::TruncatedChunk)?;
            if offset > riff_end {
                return Err(WavError::TruncatedChunk);
            }
        }

        if !format_seen {
            return Err(WavError::MissingFormat);
        }
        let data = data.ok_or(WavError::MissingData)?;
        Ok(Self { data })
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.data.len() / 2
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[inline]
    pub fn sample(&self, frame: usize) -> Option<i16> {
        let byte = frame.checked_mul(2)?;
        let pair = self.data.get(byte..byte + 2)?;
        Some(i16::from_le_bytes([pair[0], pair[1]]))
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleId {
    Kick,
    OpenHat,
}

impl SampleId {
    const fn index(self) -> usize {
        match self {
            Self::Kick => KICK_INDEX,
            Self::OpenHat => OPEN_HAT_INDEX,
        }
    }

    const fn for_pad(pad: usize) -> Self {
        if pad < PAD_COUNT / 2 {
            Self::Kick
        } else {
            Self::OpenHat
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Voice {
    pub sample: SampleId,
    pub cursor: usize,
    pub playing: bool,
}

impl Voice {
    pub const fn new(sample: SampleId) -> Self {
        Self {
            sample,
            cursor: 0,
            playing: false,
        }
    }

    #[inline]
    pub fn trigger(&mut self) {
        self.cursor = 0;
        self.playing = true;
    }

    fn next(&mut self, samples: &[WavPcm16<'_>; SAMPLE_COUNT]) -> i16 {
        if !self.playing {
            return 0;
        }
        let Some(sample) = samples[self.sample.index()].sample(self.cursor) else {
            self.playing = false;
            return 0;
        };
        self.cursor += 1;
        if self.cursor >= samples[self.sample.index()].len() {
            self.playing = false;
        }
        sample
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PadState {
    pub beats_per_interval: u16,
    pub tick_ordinal: u128,
    pub next_frame: Option<u64>,
    pub voice: Voice,
}

impl PadState {
    const fn new(pad: usize) -> Self {
        Self {
            beats_per_interval: 0,
            tick_ordinal: 0,
            next_frame: None,
            voice: Voice::new(SampleId::for_pad(pad)),
        }
    }
}

/// Per-block events produced while rendering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderReport {
    pub latest_visual_triggers: [Option<u64>; PAD_COUNT],
    pub audible_trigger_counts: [u16; SAMPLE_COUNT],
}

/// Stateful conversion from signed PCM to Raspberry Pi's PIO PWM command.
///
/// Copyright (c) 2020 Raspberry Pi (Trading) Ltd.
/// SPDX-License-Identifier: BSD-3-Clause
///
/// Adapted from pico-extras' `encode_samples_dither` implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct DitherEncoder {
    error: u16,
}

impl DitherEncoder {
    pub const fn new() -> Self {
        Self { error: 0 }
    }

    #[inline]
    pub fn encode(&mut self, sample: i16) -> u32 {
        let unsigned = (i32::from(sample) + 32_768) as u32;
        let quantized = unsigned >> 9;
        let fraction = unsigned & PWM_FRACTION_MASK;
        let mut command = quantized | ((PWM_QUANTIZED_MAX - quantized) << 7);
        let mut error = u32::from(self.error);

        for cycle in 0..PWM_DITHER_CYCLES {
            error += fraction;
            if error >= 512 {
                error -= 512;
                command |= 1 << (PWM_COMMAND_BITS + cycle);
            }
        }

        self.error = error as u16;
        command
    }

    pub const fn error(&self) -> u16 {
        self.error
    }
}

pub struct Sequencer<'a> {
    samples: [WavPcm16<'a>; SAMPLE_COUNT],
    pads: [PadState; PAD_COUNT],
    patterns: [Pattern; PAD_COUNT],
    base_interval_ms: u32,
    dither: DitherEncoder,
}

impl<'a> Sequencer<'a> {
    pub fn new(kick: WavPcm16<'a>, open_hat: WavPcm16<'a>) -> Self {
        Self {
            samples: [kick, open_hat],
            pads: core::array::from_fn(PadState::new),
            patterns: [Pattern::all_enabled(); PAD_COUNT],
            base_interval_ms: DEFAULT_BASE_INTERVAL_MS,
            dither: DitherEncoder::new(),
        }
    }

    pub fn pads(&self) -> &[PadState; PAD_COUNT] {
        &self.pads
    }

    pub const fn base_interval_ms(&self) -> u32 {
        self.base_interval_ms
    }

    pub fn set_pattern(&mut self, pad: usize, pattern: Pattern) -> bool {
        let Some(destination) = self.patterns.get_mut(pad) else {
            return false;
        };
        *destination = pattern;
        true
    }

    pub fn pattern(&self, pad: usize) -> Option<&Pattern> {
        self.patterns.get(pad)
    }

    /// Apply the base interval and per-pad beat multipliers at a render boundary.
    ///
    /// Changed timing is aligned to the global sample epoch and begins at the
    /// first tick strictly after `from_frame`. Unchanged timing retains phase.
    pub fn apply_timing(
        &mut self,
        beats: &[u16; PAD_COUNT],
        base_interval_ms: u32,
        from_frame: u64,
    ) {
        let base_interval_ms = base_interval_ms.max(MIN_BASE_INTERVAL_MS);
        let base_changed = self.base_interval_ms != base_interval_ms;
        self.base_interval_ms = base_interval_ms;

        for (pad, requested) in self.pads.iter_mut().zip(beats.iter().copied()) {
            let beats_per_interval = requested.min(MAX_BEAT_MULTIPLIER);
            if !base_changed && pad.beats_per_interval == beats_per_interval {
                continue;
            }

            pad.beats_per_interval = beats_per_interval;
            if beats_per_interval == 0 {
                pad.tick_ordinal = 0;
                pad.next_frame = None;
            } else {
                let ordinal = next_ordinal_after(from_frame, beats_per_interval, base_interval_ms);
                pad.tick_ordinal = ordinal;
                pad.next_frame = Some(frame_for_tick(
                    ordinal,
                    beats_per_interval,
                    base_interval_ms,
                ));
            }
        }
    }

    /// Render a block of PIO PWM commands beginning at an absolute frame.
    pub fn render(&mut self, start_frame: u64, output: &mut [u32]) -> RenderReport {
        let mut report = RenderReport::default();
        for (offset, word) in output.iter_mut().enumerate() {
            let frame = start_frame.wrapping_add(offset as u64);
            let mixed = self.render_pcm_frame(frame, &mut report);
            *word = self.dither.encode(mixed);
        }
        report
    }

    fn render_pcm_frame(&mut self, frame: u64, report: &mut RenderReport) -> i16 {
        let mut sample_triggered = [false; SAMPLE_COUNT];

        for (pad_index, pad) in self.pads.iter_mut().enumerate() {
            let mut enabled_step_due = false;
            while pad
                .next_frame
                .is_some_and(|next| frame_has_reached(frame, next))
            {
                let division = pad.beats_per_interval;
                let logical_step = pad.tick_ordinal.wrapping_sub(1) % u128::from(division);
                enabled_step_due |= self.patterns[pad_index]
                    .step_enabled(logical_step as u16, division)
                    .unwrap_or(false);

                pad.tick_ordinal = pad.tick_ordinal.wrapping_add(1);
                pad.next_frame = Some(frame_for_tick(
                    pad.tick_ordinal,
                    division,
                    self.base_interval_ms,
                ));
            }

            if enabled_step_due {
                report.latest_visual_triggers[pad_index] = Some(frame);
                let sample_index = pad.voice.sample.index();
                if !sample_triggered[sample_index] {
                    pad.voice.trigger();
                    sample_triggered[sample_index] = true;
                    report.audible_trigger_counts[sample_index] =
                        report.audible_trigger_counts[sample_index].saturating_add(1);
                }
            }
        }

        let mut total = 0_i32;
        for pad in &mut self.pads {
            total = total.saturating_add(i32::from(pad.voice.next(&self.samples)));
        }
        saturating_i16(total)
    }
}

fn next_ordinal_after(frame: u64, beats_per_interval: u16, base_interval_ms: u32) -> u128 {
    let numerator = u128::from(frame) * 1_000 * u128::from(beats_per_interval);
    let denominator = u128::from(SAMPLE_RATE) * u128::from(base_interval_ms);
    numerator / denominator + 1
}

fn frame_for_tick(ordinal: u128, beats_per_interval: u16, base_interval_ms: u32) -> u64 {
    if beats_per_interval == 0 {
        return u64::MAX;
    }
    let numerator = ordinal * u128::from(SAMPLE_RATE) * u128::from(base_interval_ms);
    let denominator = 1_000 * u128::from(beats_per_interval);
    numerator.div_ceil(denominator) as u64
}

/// Compare wrapping playback-frame counters. Scheduled events are always less
/// than half a `u64` cycle ahead, making this ordering unambiguous.
#[inline]
const fn frame_has_reached(frame: u64, deadline: u64) -> bool {
    frame.wrapping_sub(deadline) < (1_u64 << 63)
}

#[inline]
pub const fn saturating_i16(value: i32) -> i16 {
    if value > i16::MAX as i32 {
        i16::MAX
    } else if value < i16::MIN as i32 {
        i16::MIN
    } else {
        value as i16
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SharedState {
    pub desired_beats: [u16; PAD_COUNT],
    pub base_interval_ms: u32,
    pub led_brightness_percent: u8,
    pub playback_frame: u64,
    pub latest_trigger_frames: [u64; PAD_COUNT],
    pub underrun_count: u32,
    patterns: [Pattern; PAD_COUNT],
    pattern_dirty_mask: u16,
    pub pattern_revision: u32,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            desired_beats: [0; PAD_COUNT],
            base_interval_ms: DEFAULT_BASE_INTERVAL_MS,
            led_brightness_percent: DEFAULT_LED_BRIGHTNESS_PERCENT,
            playback_frame: 0,
            latest_trigger_frames: [0; PAD_COUNT],
            underrun_count: 0,
            patterns: [Pattern::all_enabled(); PAD_COUNT],
            pattern_dirty_mask: 0,
            pattern_revision: 0,
        }
    }
}

impl SharedState {
    pub fn pattern(&self, pad: usize) -> Option<&Pattern> {
        self.patterns.get(pad)
    }

    pub fn toggle_pattern_step(&mut self, pad: usize, step: u16) -> Option<bool> {
        let division = *self.desired_beats.get(pad)?;
        let enabled = self.patterns.get_mut(pad)?.toggle_step(step, division)?;
        self.pattern_dirty_mask |= 1 << pad;
        self.pattern_revision = self.pattern_revision.wrapping_add(1);
        Some(enabled)
    }

    pub fn take_pattern_dirty_mask(&mut self) -> u16 {
        let dirty = self.pattern_dirty_mask;
        self.pattern_dirty_mask = 0;
        dirty
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyChanges {
    pub pressed: u16,
    pub released: u16,
}

/// Debounces twelve active-high logical key bits using consecutive samples.
pub struct KeyDebouncer {
    threshold: u8,
    stable_mask: u16,
    counters: [u8; PAD_COUNT],
}

impl KeyDebouncer {
    pub fn new(stable_samples: u8) -> Self {
        Self {
            threshold: stable_samples.max(1),
            stable_mask: 0,
            counters: [0; PAD_COUNT],
        }
    }

    pub fn stable_mask(&self) -> u16 {
        self.stable_mask
    }

    pub fn update(&mut self, raw_mask: u16) -> KeyChanges {
        let mut changes = KeyChanges::default();
        for pad in 0..PAD_COUNT {
            let bit = 1_u16 << pad;
            let raw = raw_mask & bit != 0;
            let stable = self.stable_mask & bit != 0;
            if raw == stable {
                self.counters[pad] = 0;
                continue;
            }

            self.counters[pad] = self.counters[pad].saturating_add(1);
            if self.counters[pad] >= self.threshold {
                self.counters[pad] = 0;
                if raw {
                    self.stable_mask |= bit;
                    changes.pressed |= bit;
                } else {
                    self.stable_mask &= !bit;
                    changes.released |= bit;
                }
            }
        }
        changes
    }
}

/// Tracks the oldest pressed key that is still held as the primary pad.
pub struct HeldPadSelection {
    held_mask: u16,
    press_order: [u32; PAD_COUNT],
    sequence: u32,
}

impl HeldPadSelection {
    pub const fn new() -> Self {
        Self {
            held_mask: 0,
            press_order: [0; PAD_COUNT],
            sequence: 0,
        }
    }

    pub fn apply(&mut self, changes: KeyChanges) {
        self.held_mask &= !changes.released;
        for pad in 0..PAD_COUNT {
            let bit = 1_u16 << pad;
            if changes.pressed & bit != 0 {
                self.sequence = self.sequence.wrapping_add(1);
                if self.sequence == 0 {
                    self.renumber();
                    self.sequence = PAD_COUNT as u32 + 1;
                }
                self.press_order[pad] = self.sequence;
                self.held_mask |= bit;
            }
        }
    }

    pub fn selected(&self) -> Option<usize> {
        let mut selected = None;
        let mut oldest = u32::MAX;
        for pad in 0..PAD_COUNT {
            if self.held_mask & (1 << pad) != 0 && self.press_order[pad] < oldest {
                oldest = self.press_order[pad];
                selected = Some(pad);
            }
        }
        selected
    }

    pub const fn held_mask(&self) -> u16 {
        self.held_mask
    }

    fn renumber(&mut self) {
        // Overflow is practically unreachable; preserving relative pad order is
        // sufficient if it ever occurs.
        let mut next = 1_u32;
        for pad in 0..PAD_COUNT {
            if self.held_mask & (1 << pad) != 0 {
                self.press_order[pad] = next;
                next += 1;
            }
        }
    }
}

impl Default for HeldPadSelection {
    fn default() -> Self {
        Self::new()
    }
}

pub fn adjust_beat_multiplier(current: u16, delta: i32) -> u16 {
    let adjusted = i32::from(current).saturating_add(delta);
    adjusted.clamp(0, i32::from(MAX_BEAT_MULTIPLIER)) as u16
}

pub fn adjust_base_interval(current_ms: u32, delta_steps: i32) -> u32 {
    let delta_ms = delta_steps
        .unsigned_abs()
        .saturating_mul(BASE_INTERVAL_STEP_MS);
    if delta_steps.is_negative() {
        current_ms
            .saturating_sub(delta_ms)
            .max(MIN_BASE_INTERVAL_MS)
    } else {
        current_ms.saturating_add(delta_ms)
    }
}

pub fn adjust_led_brightness(current_percent: u8, delta: i32) -> u8 {
    i32::from(current_percent)
        .saturating_add(delta)
        .clamp(0, 100) as u8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternEditorAction {
    Entered,
    Toggle { pad: usize, step: u16 },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PatternEditorState {
    pub active: bool,
    pub cursor: u16,
}

impl PatternEditorState {
    pub const fn new() -> Self {
        Self {
            active: false,
            cursor: 0,
        }
    }

    pub fn update_primary(&mut self, primary: Option<usize>, division: u16) {
        if primary.is_none() {
            self.active = false;
            self.cursor = 0;
        } else if self.active {
            self.cursor = wrap_pattern_cursor(self.cursor, division, 0);
        }
    }

    pub fn button_pressed(
        &mut self,
        primary: Option<usize>,
        division: u16,
    ) -> Option<PatternEditorAction> {
        let pad = primary?;
        if !self.active {
            self.active = true;
            self.cursor = 0;
            return Some(PatternEditorAction::Entered);
        }
        if division == 0 {
            return None;
        }
        self.cursor %= division;
        Some(PatternEditorAction::Toggle {
            pad,
            step: self.cursor,
        })
    }

    pub fn scroll(&mut self, division: u16, delta: i32) {
        if self.active {
            self.cursor = wrap_pattern_cursor(self.cursor, division, delta);
        }
    }
}

pub fn wrap_pattern_cursor(cursor: u16, division: u16, delta: i32) -> u16 {
    if division == 0 {
        return 0;
    }
    i32::from(cursor)
        .saturating_add(delta)
        .rem_euclid(i32::from(division)) as u16
}

pub fn pattern_window_start(cursor: u16, division: u16, visible_rows: u16) -> u16 {
    if visible_rows == 0 || division <= visible_rows {
        return 0;
    }
    cursor
        .saturating_sub(visible_rows / 2)
        .min(division - visible_rows)
}

/// Accelerate a direction delta when consecutive detents arrive quickly.
pub fn accelerated_encoder_delta(direction: i32, elapsed_ms: Option<u64>) -> i32 {
    let multiplier = if elapsed_ms.is_some_and(|elapsed| elapsed <= FAST_ENCODER_THRESHOLD_MS) {
        FAST_ENCODER_MULTIPLIER
    } else {
        1
    };
    direction.saturating_mul(multiplier)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncoderTarget {
    BaseInterval,
    LedBrightness,
    Pad(usize),
    PatternStep(usize),
}

impl EncoderTarget {
    pub const fn for_controls(
        selected_pad: Option<usize>,
        encoder_pressed: bool,
        pattern_mode: bool,
    ) -> Self {
        match selected_pad {
            Some(pad) if pattern_mode => Self::PatternStep(pad),
            Some(pad) => Self::Pad(pad),
            None if encoder_pressed => Self::LedBrightness,
            None => Self::BaseInterval,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EncoderAcceleration {
    last_event: Option<(u64, EncoderTarget, i32)>,
}

impl EncoderAcceleration {
    pub const fn new() -> Self {
        Self { last_event: None }
    }

    pub fn update(&mut self, now_ms: u64, target: EncoderTarget, direction: i32) -> i32 {
        let elapsed_ms = match self.last_event {
            Some((last_ms, last_target, last_direction))
                if last_target == target && last_direction == direction =>
            {
                now_ms.checked_sub(last_ms)
            }
            _ => None,
        };
        self.last_event = Some((now_ms, target, direction));
        accelerated_encoder_delta(direction, elapsed_ms)
    }
}

/// Apply one or more encoder steps to the currently selected timing control.
pub fn apply_encoder_delta(state: &mut SharedState, target: EncoderTarget, delta: i32) {
    match target {
        EncoderTarget::Pad(pad) if pad < PAD_COUNT => {
            state.desired_beats[pad] = adjust_beat_multiplier(state.desired_beats[pad], delta);
        }
        EncoderTarget::Pad(_) | EncoderTarget::PatternStep(_) => {}
        EncoderTarget::BaseInterval => {
            state.base_interval_ms = adjust_base_interval(state.base_interval_ms, delta);
        }
        EncoderTarget::LedBrightness => {
            state.led_brightness_percent =
                adjust_led_brightness(state.led_brightness_percent, delta);
        }
    }
}

pub fn scale_color(color: (u8, u8, u8), brightness_percent: u8) -> (u8, u8, u8) {
    let brightness = u16::from(brightness_percent.min(100));
    let scale = |channel: u8| ((u16::from(channel) * brightness + 50) / 100) as u8;
    (scale(color.0), scale(color.1), scale(color.2))
}

/// CircuitPython `rainbowio.colorwheel` for an 8-bit wheel position.
pub fn colorwheel(position: u8) -> (u8, u8, u8) {
    if position < 85 {
        let offset = position * 3;
        (255 - offset, offset, 0)
    } else if position < 170 {
        let offset = (position - 85) * 3;
        (0, 255 - offset, offset)
    } else {
        let offset = (position - 170) * 3;
        (offset, 0, 255 - offset)
    }
}

/// Returns whether a pad's playback-aligned LED pulse should currently be on.
pub fn led_pulse_active(playback_frame: u64, trigger_frame: u64, pulse_frames: u32) -> bool {
    if trigger_frame == 0 {
        return false;
    }
    playback_frame.wrapping_sub(trigger_frame) < u64::from(pulse_frames)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const KICK_WAV: &[u8] = include_bytes!("../samples/00_kick02.wav");
    const HAT_WAV: &[u8] = include_bytes!("../samples/02_ho02.wav");

    fn wav(samples: &[i16], extra_chunk: bool) -> Vec<u8> {
        let extra_len = if extra_chunk { 10 } else { 0 };
        let riff_len = 4 + (8 + 16) + extra_len + 8 + samples.len() * 2;
        let mut bytes = Vec::with_capacity(riff_len + 8);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(riff_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        if extra_chunk {
            bytes.extend_from_slice(b"JUNK");
            bytes.extend_from_slice(&1_u32.to_le_bytes());
            bytes.push(0xaa);
            bytes.push(0); // RIFF chunks are word padded.
        }
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&((samples.len() * 2) as u32).to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parses_repository_samples() {
        let kick = WavPcm16::parse(KICK_WAV).unwrap();
        let hat = WavPcm16::parse(HAT_WAV).unwrap();
        assert_eq!(kick.len(), 11_265);
        assert_eq!(hat.len(), 6_852);
        assert!(kick.sample(0).is_some());
        assert_eq!(kick.sample(kick.len()), None);
    }

    #[test]
    fn parses_unknown_padded_chunks_and_signed_samples() {
        let bytes = wav(&[i16::MIN, -1, 0, i16::MAX], true);
        let parsed = WavPcm16::parse(&bytes).unwrap();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed.sample(0), Some(i16::MIN));
        assert_eq!(parsed.sample(1), Some(-1));
        assert_eq!(parsed.sample(3), Some(i16::MAX));
    }

    #[test]
    fn rejects_invalid_and_truncated_wavs() {
        assert_eq!(WavPcm16::parse(b"short").unwrap_err(), WavError::TooShort);
        let mut bytes = wav(&[1, 2], false);
        bytes[0] = b'X';
        assert_eq!(WavPcm16::parse(&bytes).unwrap_err(), WavError::InvalidRiff);
        let mut bytes = wav(&[1, 2], false);
        bytes.truncate(bytes.len() - 1);
        assert_eq!(
            WavPcm16::parse(&bytes).unwrap_err(),
            WavError::TruncatedRiff
        );
        let mut bytes = wav(&[1, 2], false);
        bytes[20] = 2; // two channels
        assert_eq!(
            WavPcm16::parse(&bytes).unwrap_err(),
            WavError::UnsupportedFormat
        );

        let mut missing_format = wav(&[1], false);
        missing_format[12..16].copy_from_slice(b"JUNK");
        assert_eq!(
            WavPcm16::parse(&missing_format).unwrap_err(),
            WavError::MissingFormat
        );

        let mut missing_data = wav(&[1], false);
        let data_offset = missing_data.len() - 10;
        missing_data[data_offset..data_offset + 4].copy_from_slice(b"JUNK");
        assert_eq!(
            WavPcm16::parse(&missing_data).unwrap_err(),
            WavError::MissingData
        );

        let mut odd_data = wav(&[1], false);
        odd_data[40..44].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(
            WavPcm16::parse(&odd_data).unwrap_err(),
            WavError::OddDataLength
        );
    }

    #[test]
    fn voice_retriggers_and_stops_at_sample_end() {
        let kick_bytes = wav(&[10, 20], false);
        let hat_bytes = wav(&[-10], false);
        let samples = [
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        ];
        let mut voice = Voice::new(SampleId::Kick);

        assert_eq!(voice.next(&samples), 0);
        voice.trigger();
        assert_eq!(voice.next(&samples), 10);
        assert!(voice.playing);
        assert_eq!(voice.next(&samples), 20);
        assert!(!voice.playing);
        assert_eq!(voice.next(&samples), 0);
        voice.trigger();
        assert_eq!(voice.next(&samples), 10);
    }

    #[test]
    fn pads_map_to_samples_and_mixing_saturates() {
        let kick_bytes = wav(&[20_000], false);
        let hat_bytes = wav(&[-20_000], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );

        for pad in 0..6 {
            assert_eq!(sequencer.pads[pad].voice.sample, SampleId::Kick);
        }
        for pad in 6..PAD_COUNT {
            assert_eq!(sequencer.pads[pad].voice.sample, SampleId::OpenHat);
        }

        sequencer.pads[0].voice.trigger();
        sequencer.pads[1].voice.trigger();
        assert_eq!(
            sequencer.render_pcm_frame(0, &mut RenderReport::default()),
            i16::MAX
        );

        sequencer.pads[6].voice.trigger();
        sequencer.pads[7].voice.trigger();
        assert_eq!(
            sequencer.render_pcm_frame(1, &mut RenderReport::default()),
            i16::MIN
        );
    }

    #[test]
    fn dither_has_centered_and_full_scale_golden_values() {
        let mut encoder = DitherEncoder::new();
        assert_eq!(encoder.encode(0), SILENCE_PWM_WORD);
        assert_eq!(encoder.error(), 0);
        assert_eq!(encoder.encode(i16::MIN), 127 << 7);

        let mut encoder = DitherEncoder::new();
        let expected = 127 | (((1_u32 << 16) - 2) << 14);
        assert_eq!(encoder.encode(i16::MAX), expected);
        assert_eq!(encoder.error(), 496);
    }

    #[test]
    fn dither_error_is_continuous_across_blocks() {
        let values = [-12_345, 123, 30_000, -1, 0, 9_999];
        let mut continuous = DitherEncoder::new();
        let mut expected = [0_u32; 6];
        for (word, sample) in expected.iter_mut().zip(values) {
            *word = continuous.encode(sample);
        }

        let mut split = DitherEncoder::new();
        let mut actual = [0_u32; 6];
        for index in 0..3 {
            actual[index] = split.encode(values[index]);
        }
        for index in 3..6 {
            actual[index] = split.encode(values[index]);
        }
        assert_eq!(actual, expected);
        assert_eq!(split.error(), continuous.error());
    }

    #[test]
    fn patterns_use_a_fixed_2048_bit_phase_grid() {
        assert_eq!(core::mem::size_of::<Pattern>(), PATTERN_BYTES);

        let pattern = Pattern::default();
        assert_eq!(pattern.bit(0), Some(true));
        assert_eq!(pattern.bit(PATTERN_BITS - 1), Some(true));
        assert_eq!(pattern.bit(PATTERN_BITS), None);

        assert_eq!(pattern_step_range(0, 1), Some((0, PATTERN_BITS)));
        assert_eq!(pattern_step_range(0, 3), Some((0, 682)));
        assert_eq!(pattern_step_range(1, 3), Some((682, 1_365)));
        assert_eq!(pattern_step_range(2, 3), Some((1_365, PATTERN_BITS)));
        assert_eq!(pattern_step_range(731, 2_048), Some((731, 732)));
        assert_eq!(pattern_step_range(0, 0), None);
        assert_eq!(pattern_step_range(3, 3), None);
    }

    #[test]
    fn pattern_edits_fill_ranges_and_resample_their_first_bits() {
        let mut pattern = Pattern::default();
        assert_eq!(pattern.toggle_step(0, 2), Some(false));

        // Turning off the first half at division 2 also turns off the first
        // two entries when the same fixed grid is sampled at division 4.
        assert_eq!(pattern.step_enabled(0, 4), Some(false));
        assert_eq!(pattern.step_enabled(1, 4), Some(false));
        assert_eq!(pattern.step_enabled(2, 4), Some(true));
        assert_eq!(pattern.step_enabled(3, 4), Some(true));
        assert_eq!(pattern.bit(1_023), Some(false));
        assert_eq!(pattern.bit(1_024), Some(true));

        // A later coarse edit replaces every finer bit in that logical range.
        assert!(pattern.set_step_enabled(1, 3, false));
        for bit in 682..1_365 {
            assert_eq!(pattern.bit(bit), Some(false));
        }
        assert_eq!(pattern.toggle_step(1, 3), Some(true));
        for bit in 682..1_365 {
            assert_eq!(pattern.bit(bit), Some(true));
        }
    }

    #[test]
    fn shared_patterns_track_audio_sync_and_display_revisions() {
        let mut state = SharedState::default();
        state.desired_beats[2] = 4;

        assert_eq!(state.toggle_pattern_step(2, 1), Some(false));
        assert_eq!(state.pattern_revision, 1);
        assert_eq!(state.pattern(2).unwrap().step_enabled(1, 4), Some(false));
        assert_eq!(state.take_pattern_dirty_mask(), 1 << 2);
        assert_eq!(state.take_pattern_dirty_mask(), 0);

        assert_eq!(state.toggle_pattern_step(2, 4), None);
        assert_eq!(state.pattern_revision, 1);
        assert_eq!(state.take_pattern_dirty_mask(), 0);
    }

    #[test]
    fn scheduling_is_global_phase_aligned_and_zero_stops_new_triggers() {
        let kick_bytes = wav(&[1, 2, 3], false);
        let hat_bytes = wav(&[4, 5, 6], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut beats = [0; PAD_COUNT];
        beats[0] = 1;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);
        assert_eq!(sequencer.pads()[0].next_frame, Some(22_050));

        beats[1] = 2;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);
        assert_eq!(sequencer.pads()[1].next_frame, Some(11_025));

        beats[2] = 1_000;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);
        assert_eq!(sequencer.pads()[2].next_frame, Some(23));

        beats[0] = 3;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 10_000);
        assert_eq!(sequencer.pads()[0].next_frame, Some(14_700));

        beats[0] = 0;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 10_001);
        assert_eq!(sequencer.pads()[0].next_frame, None);
    }

    #[test]
    fn disabled_pattern_ticks_advance_without_triggering_audio_or_visuals() {
        let kick_bytes = wav(&[100], false);
        let hat_bytes = wav(&[200], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut pattern = Pattern::default();
        assert!(pattern.set_step_enabled(0, 4, false));
        assert!(sequencer.set_pattern(0, pattern));

        let mut beats = [0; PAD_COUNT];
        beats[0] = 4;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);

        let mut output = [0_u32; 1];
        let disabled = sequencer.render(5_513, &mut output);
        assert_eq!(disabled.latest_visual_triggers[0], None);
        assert_eq!(disabled.audible_trigger_counts[0], 0);
        assert_eq!(sequencer.pads()[0].tick_ordinal, 2);
        assert_eq!(sequencer.pads()[0].next_frame, Some(11_025));

        let enabled = sequencer.render(11_025, &mut output);
        assert_eq!(enabled.latest_visual_triggers[0], Some(11_025));
        assert_eq!(enabled.audible_trigger_counts[0], 1);
    }

    #[test]
    fn changing_base_interval_reschedules_all_enabled_pads() {
        let kick_bytes = wav(&[1, 2, 3], false);
        let hat_bytes = wav(&[4, 5, 6], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut beats = [0; PAD_COUNT];
        beats[0] = 1;
        beats[6] = 2;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);
        sequencer.pads[0].voice.trigger();

        sequencer.apply_timing(&beats, 500, 10_000);
        assert_eq!(sequencer.base_interval_ms(), 500);
        assert_eq!(sequencer.pads()[0].next_frame, Some(11_025));
        assert_eq!(sequencer.pads()[6].next_frame, Some(11_025));
        assert!(sequencer.pads()[0].voice.playing);

        // Changing timing exactly on a new-grid boundary selects the following one.
        sequencer.apply_timing(&beats, 1_000, 11_025);
        sequencer.apply_timing(&beats, 500, 11_025);
        assert_eq!(sequencer.pads()[0].next_frame, Some(22_050));
        assert_eq!(sequencer.pads()[1].next_frame, None);
    }

    #[test]
    fn long_base_interval_supports_large_polyrhythm_divisions() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut beats = [0; PAD_COUNT];
        beats[0] = 71;
        beats[6] = 73;

        // 71 divisions across 106.5 seconds gives exactly 40 BPM.
        sequencer.apply_timing(&beats, 106_500, 0);
        assert_eq!(sequencer.base_interval_ms(), 106_500);
        assert_eq!(sequencer.pads()[0].next_frame, Some(33_075));
        assert_eq!(sequencer.pads()[6].next_frame, Some(32_169));
    }

    #[test]
    fn scheduler_is_safe_across_playback_frame_wrap() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut beats = [0; PAD_COUNT];
        beats[0] = 1_000;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, u64::MAX - 10);
        let deadline = sequencer.pads()[0].next_frame.unwrap();
        assert!(
            deadline < 32,
            "the next deadline should wrap to the new epoch"
        );

        let mut output = [0_u32; 64];
        let report = sequencer.render(u64::MAX - 10, &mut output);
        assert!(report.audible_trigger_counts[0] >= 2);
        assert!(sequencer.pads()[0].next_frame.unwrap() > deadline);
    }

    #[test]
    fn fastest_supported_timing_never_builds_a_frame_backlog() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut beats = [0; PAD_COUNT];
        beats[0] = MAX_BEAT_MULTIPLIER;
        sequencer.apply_timing(&beats, MIN_BASE_INTERVAL_MS, 0);

        let mut output = [0_u32; AUDIO_BLOCK_FRAMES];
        let report = sequencer.render(0, &mut output);
        let next = sequencer.pads()[0].next_frame.unwrap();
        assert_eq!(sequencer.pads()[0].tick_ordinal, 236);
        assert_eq!(report.audible_trigger_counts[0], 127);
        assert!(!frame_has_reached((AUDIO_BLOCK_FRAMES - 1) as u64, next));
    }

    #[test]
    fn coalesced_ticks_trigger_when_any_due_pattern_entry_is_enabled() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut pattern = Pattern::default();
        pattern.fill(false);
        assert!(pattern.set_step_enabled(2, MAX_BEAT_MULTIPLIER, true));
        assert!(sequencer.set_pattern(0, pattern));

        let mut beats = [0; PAD_COUNT];
        beats[0] = MAX_BEAT_MULTIPLIER;
        sequencer.apply_timing(&beats, MIN_BASE_INTERVAL_MS, 0);

        let mut output = [0_u32; 1];
        let first_frame = sequencer.render(1, &mut output);
        assert_eq!(first_frame.audible_trigger_counts[0], 0);
        assert_eq!(sequencer.pads()[0].tick_ordinal, 2);

        // Ordinals 2 and 3 both land on frame 2. Step 1 is disabled and
        // step 2 is enabled, so the pair coalesces into exactly one trigger.
        let coalesced = sequencer.render(2, &mut output);
        assert_eq!(coalesced.latest_visual_triggers[0], Some(2));
        assert_eq!(coalesced.audible_trigger_counts[0], 1);
        assert_eq!(sequencer.pads()[0].tick_ordinal, 4);
    }

    #[test]
    fn rendering_suppresses_duplicate_samples_but_not_visuals() {
        let kick_bytes = wav(&[100, 50], false);
        let hat_bytes = wav(&[200, 100], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut beats = [0; PAD_COUNT];
        beats[0] = 1_000;
        beats[1] = 1_000;
        beats[6] = 1_000;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);

        let mut output = [0_u32; 24];
        let report = sequencer.render(0, &mut output);
        assert_eq!(report.latest_visual_triggers[0], Some(23));
        assert_eq!(report.latest_visual_triggers[1], Some(23));
        assert_eq!(report.latest_visual_triggers[6], Some(23));
        assert_eq!(report.audible_trigger_counts, [1, 1]);
        assert_eq!(sequencer.pads()[0].voice.cursor, 1);
        assert_eq!(sequencer.pads()[1].voice.cursor, 0);
        assert_eq!(sequencer.pads()[6].voice.cursor, 1);
    }

    #[test]
    fn long_run_trigger_counts_do_not_drift() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut beats = [0; PAD_COUNT];
        beats[0] = 3;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);

        let mut total = 0_u32;
        let mut buffer = [0_u32; AUDIO_BLOCK_FRAMES];
        let end = SAMPLE_RATE as u64 * 10;
        let mut frame = 0_u64;
        while frame < end {
            let count = ((end - frame) as usize).min(AUDIO_BLOCK_FRAMES);
            let report = sequencer.render(frame, &mut buffer[..count]);
            total += u32::from(report.audible_trigger_counts[0]);
            frame += count as u64;
        }
        // The boundary at exactly ten seconds belongs to the next frame.
        assert_eq!(total, 29);
        assert_eq!(sequencer.pads()[0].next_frame, Some(end));
    }

    #[test]
    fn mixer_saturates_both_directions() {
        assert_eq!(saturating_i16(100), 100);
        assert_eq!(saturating_i16(100_000), i16::MAX);
        assert_eq!(saturating_i16(-100_000), i16::MIN);
    }

    #[test]
    fn keys_debounce_and_oldest_held_pad_remains_primary() {
        let mut debounce = KeyDebouncer::new(3);
        assert_eq!(debounce.update(1), KeyChanges::default());
        assert_eq!(debounce.update(1), KeyChanges::default());
        let press_zero = debounce.update(1);
        assert_eq!(press_zero.pressed, 1);

        let mut selection = HeldPadSelection::new();
        selection.apply(press_zero);
        assert_eq!(selection.selected(), Some(0));
        selection.apply(KeyChanges {
            pressed: 1 << 4,
            released: 0,
        });
        assert_eq!(selection.selected(), Some(0));
        assert_eq!(selection.held_mask(), (1 << 0) | (1 << 4));
        selection.apply(KeyChanges {
            pressed: 0,
            released: 1 << 0,
        });
        assert_eq!(selection.selected(), Some(4));

        assert_eq!(debounce.update(0), KeyChanges::default());
        assert_eq!(debounce.update(0), KeyChanges::default());
        assert_eq!(debounce.update(0).released, 1);
    }

    #[test]
    fn pattern_editor_enters_then_toggles_and_resets_only_when_all_keys_are_up() {
        let mut editor = PatternEditorState::new();
        editor.update_primary(Some(2), 4);
        assert_eq!(
            editor.button_pressed(Some(2), 4),
            Some(PatternEditorAction::Entered)
        );
        assert!(editor.active);

        editor.scroll(4, 1);
        assert_eq!(editor.cursor, 1);
        assert_eq!(
            editor.button_pressed(Some(2), 4),
            Some(PatternEditorAction::Toggle { pad: 2, step: 1 })
        );
        editor.scroll(4, 10);
        assert_eq!(editor.cursor, 3);

        // A later-held pad becomes primary only after the older key is
        // released; pattern mode itself persists until every key is released.
        editor.update_primary(Some(7), 2);
        assert!(editor.active);
        assert_eq!(editor.cursor, 1);
        assert_eq!(
            editor.button_pressed(Some(7), 2),
            Some(PatternEditorAction::Toggle { pad: 7, step: 1 })
        );

        editor.update_primary(Some(7), 0);
        assert!(editor.active);
        assert_eq!(editor.cursor, 0);
        assert_eq!(editor.button_pressed(Some(7), 0), None);

        editor.update_primary(None, 0);
        assert_eq!(editor, PatternEditorState::new());
    }

    #[test]
    fn pattern_cursor_wraps_and_display_window_tracks_it() {
        assert_eq!(wrap_pattern_cursor(0, 8, -1), 7);
        assert_eq!(wrap_pattern_cursor(7, 8, 1), 0);
        assert_eq!(wrap_pattern_cursor(1, 8, 10), 3);
        assert_eq!(wrap_pattern_cursor(123, 0, 10), 0);

        assert_eq!(pattern_window_start(0, 12, 5), 0);
        assert_eq!(pattern_window_start(4, 12, 5), 2);
        assert_eq!(pattern_window_start(11, 12, 5), 7);
        assert_eq!(pattern_window_start(3, 4, 5), 0);
        assert_eq!(pattern_window_start(3, 12, 0), 0);
    }

    #[test]
    fn timing_palette_and_led_helpers_are_bounded() {
        assert_eq!(adjust_beat_multiplier(0, -1), 0);
        assert_eq!(
            adjust_beat_multiplier(MAX_BEAT_MULTIPLIER - 1, 10),
            MAX_BEAT_MULTIPLIER
        );
        assert_eq!(adjust_base_interval(1_000, 1), 1_010);
        assert_eq!(adjust_base_interval(1_000, -1), 990);
        assert_eq!(adjust_base_interval(MIN_BASE_INTERVAL_MS, -1), 50);
        assert_eq!(adjust_base_interval(u32::MAX, 1), u32::MAX);
        assert_eq!(accelerated_encoder_delta(1, None), 1);
        assert_eq!(accelerated_encoder_delta(1, Some(76)), 1);
        assert_eq!(accelerated_encoder_delta(1, Some(75)), 10);
        assert_eq!(accelerated_encoder_delta(-1, Some(20)), -10);

        assert_eq!(
            EncoderTarget::for_controls(None, false, false),
            EncoderTarget::BaseInterval
        );
        assert_eq!(
            EncoderTarget::for_controls(None, true, false),
            EncoderTarget::LedBrightness
        );
        assert_eq!(
            EncoderTarget::for_controls(Some(3), true, false),
            EncoderTarget::Pad(3)
        );
        assert_eq!(
            EncoderTarget::for_controls(Some(3), true, true),
            EncoderTarget::PatternStep(3)
        );

        let mut acceleration = EncoderAcceleration::new();
        assert_eq!(acceleration.update(1_000, EncoderTarget::Pad(3), 1), 1);
        assert_eq!(acceleration.update(1_050, EncoderTarget::Pad(3), 1), 10);
        assert_eq!(
            acceleration.update(1_060, EncoderTarget::LedBrightness, 1),
            1
        );
        assert_eq!(
            acceleration.update(1_070, EncoderTarget::BaseInterval, 1),
            1
        );
        assert_eq!(
            acceleration.update(1_080, EncoderTarget::BaseInterval, -1),
            -1
        );
        assert_eq!(
            acceleration.update(1_090, EncoderTarget::BaseInterval, -1),
            -10
        );

        let mut state = SharedState::default();
        apply_encoder_delta(&mut state, EncoderTarget::BaseInterval, 1);
        assert_eq!(state.base_interval_ms, 1_010);
        apply_encoder_delta(&mut state, EncoderTarget::BaseInterval, 10);
        assert_eq!(state.base_interval_ms, 1_110);
        apply_encoder_delta(&mut state, EncoderTarget::Pad(3), 10);
        assert_eq!(state.desired_beats[3], 10);
        assert_eq!(state.base_interval_ms, 1_110);
        apply_encoder_delta(&mut state, EncoderTarget::LedBrightness, -10);
        assert_eq!(state.led_brightness_percent, 40);
        apply_encoder_delta(&mut state, EncoderTarget::LedBrightness, -1_000);
        assert_eq!(state.led_brightness_percent, 0);
        apply_encoder_delta(&mut state, EncoderTarget::LedBrightness, 1_000);
        assert_eq!(state.led_brightness_percent, 100);

        assert_eq!(scale_color((200, 100, 50), 100), (200, 100, 50));
        assert_eq!(scale_color((200, 100, 50), 50), (100, 50, 25));
        assert_eq!(scale_color((200, 100, 50), 0), (0, 0, 0));
        assert_eq!(colorwheel(0), (255, 0, 0));
        assert_eq!(colorwheel(85), (0, 255, 0));
        assert_eq!(colorwheel(170), (0, 0, 255));
        assert!(!led_pulse_active(0, 0, 2_205));
        assert!(led_pulse_active(1_100, 1_000, 2_205));
        assert!(!led_pulse_active(3_205, 1_000, 2_205));
        assert!(led_pulse_active(5, u64::MAX - 4, 20));
    }
}
