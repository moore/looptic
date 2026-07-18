#![no_std]

//! Platform-independent audio, sequencing, and UI state for LoopTic.
//!
//! The RP2040 firmware lives in `main.rs`. Keeping this module free of HAL
//! dependencies makes the timing and sample conversion code testable on a host.

pub mod flash_storage;
pub mod load_control;
pub mod sample_assets;

use load_control::{DitherQuality, LoadLevel, RenderPolicy};

pub const KEY_COUNT: usize = 12;
pub const BEAT_PAD_COUNT: usize = 9;
pub const MUTE_KEY_INDEX: usize = 9;
pub const VOLUME_KEY_INDEX: usize = 10;
pub const RETURN_KEY_INDEX: usize = 11;
pub const BEAT_PAD_MASK: u16 = (1_u16 << BEAT_PAD_COUNT) - 1;
pub const SAMPLE_RATE: u32 = 22_050;
pub const AUDIO_BLOCK_FRAMES: usize = 128;
pub const MAX_BEAT_MULTIPLIER: u16 = 256;
pub const PATTERN_BITS: usize = MAX_BEAT_MULTIPLIER as usize;
pub const PATTERN_BYTES: usize = PATTERN_BITS / 8;
pub const DEFAULT_BASE_INTERVAL_MS: u32 = 1_000;
// Missed logical ticks are coalesced to one trigger per pad and sample frame.
pub const MIN_BASE_INTERVAL_MS: u32 = 50;
pub const BASE_INTERVAL_STEP_MS: u32 = 10;
pub const FAST_ENCODER_MULTIPLIER: i32 = 10;
pub const FAST_ENCODER_THRESHOLD_MS: u64 = 40;
pub const DEFAULT_LED_BRIGHTNESS_PERCENT: u8 = 50;
pub const DEFAULT_VOLUME_PERCENT: u8 = 100;
pub const DEFAULT_TRIGGER_VOLUME_PERCENT: u8 = 100;
pub const MUTE_TAP_THRESHOLD_MS: u64 = 300;
pub const MUTE_LED_DIM_PERCENT: u8 = 20;
pub const NONSELECTED_LED_SCALE_PERCENT: u8 = 20;
pub const SAMPLE_COUNT: usize = 24;
pub const SONG_SLOT_COUNT: usize = 256;
pub const PRIMARY_VOICE_COUNT: usize = 24;
pub const FADE_TAIL_COUNT: usize = 9;
pub const DECLICK_FRAMES: u8 = 32;
pub const GAIN_RAMP_FRAMES: u8 = 64;

const DECLICK_SHIFT: u32 = DECLICK_FRAMES.trailing_zeros();
const GAIN_RAMP_SHIFT: u32 = GAIN_RAMP_FRAMES.trailing_zeros();
const _: () = assert!(DECLICK_FRAMES.is_power_of_two());
const _: () = assert!(GAIN_RAMP_FRAMES.is_power_of_two());

const DEFAULT_KICK_INDEX: usize = 16;
const DEFAULT_OPEN_HAT_INDEX: usize = 18;
const PWM_QUANTIZED_MAX: u32 = 127;
const PWM_FRACTION_MASK: u32 = 0x1ff;
const PWM_COMMAND_BITS: u32 = 14;
const PWM_DITHER_CYCLES: u32 = 16;
const COARSE_DITHER_MASKS: [u16; PWM_DITHER_CYCLES as usize + 1] = coarse_dither_masks();

const fn coarse_dither_masks() -> [u16; PWM_DITHER_CYCLES as usize + 1] {
    let mut masks = [0_u16; PWM_DITHER_CYCLES as usize + 1];
    let mut ones = 1_u32;
    while ones <= PWM_DITHER_CYCLES {
        let mut accumulator = 0_u32;
        let mut mask = 0_u16;
        let mut cycle = 0_u32;
        while cycle < PWM_DITHER_CYCLES {
            accumulator += ones;
            if accumulator >= PWM_DITHER_CYCLES {
                accumulator -= PWM_DITHER_CYCLES;
                mask |= 1 << cycle;
            }
            cycle += 1;
        }
        masks[ones as usize] = mask;
        ones += 1;
    }
    masks
}

/// Centered PWM command used while no PCM data is available.
pub const SILENCE_PWM_WORD: u32 = 64 | (63 << 7);

/// A persistent 256-slot map whose first `division` bits form the active pattern.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pattern {
    bits: [u8; PATTERN_BYTES],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternFillState {
    Empty,
    Full,
    Mixed,
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
        let index = pattern_step_index(step, division)?;
        self.bit(index)
    }

    pub fn set_step_enabled(&mut self, step: u16, division: u16, enabled: bool) -> bool {
        let Some(index) = pattern_step_index(step, division) else {
            return false;
        };
        self.set_bit(index, enabled)
    }

    pub fn fill(&mut self, enabled: bool) {
        self.bits.fill(if enabled { u8::MAX } else { 0 });
    }

    pub fn fill_state(&self) -> PatternFillState {
        let mut any_enabled = false;
        let mut any_disabled = false;
        for &byte in &self.bits {
            any_enabled |= byte != 0;
            any_disabled |= byte != u8::MAX;
            if any_enabled && any_disabled {
                return PatternFillState::Mixed;
            }
        }
        if any_enabled {
            PatternFillState::Full
        } else {
            PatternFillState::Empty
        }
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

/// Independent volume percentages for every direct pattern slot.
///
/// Enable bits live in [`Pattern`], so disabling or hiding a trigger never
/// erases its stored level. `sum` keeps the `All` row and control LED cheap to
/// render without scanning the map in a critical section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggerVolumes {
    percents: [u8; PATTERN_BITS],
    sum: u32,
}

impl TriggerVolumes {
    pub const fn all_default() -> Self {
        Self {
            percents: [DEFAULT_TRIGGER_VOLUME_PERCENT; PATTERN_BITS],
            sum: PATTERN_BITS as u32 * DEFAULT_TRIGGER_VOLUME_PERCENT as u32,
        }
    }

    pub fn percent(&self, step: usize) -> Option<u8> {
        self.percents.get(step).copied()
    }

    pub fn average_percent(&self) -> u8 {
        ((self.sum + PATTERN_BITS as u32 / 2) / PATTERN_BITS as u32) as u8
    }

    /// Adjust one stored slot by percentage points. Returns `None` for an
    /// invalid slot, otherwise the resulting (possibly already-clamped) value.
    pub fn adjust_step(&mut self, step: usize, delta: i32) -> Option<u8> {
        let percent = self.percents.get_mut(step)?;
        let previous = *percent;
        let adjusted = adjust_volume_percent(previous, delta);
        *percent = adjusted;
        self.sum = self
            .sum
            .saturating_sub(u32::from(previous))
            .saturating_add(u32::from(adjusted));
        Some(adjusted)
    }

    /// Apply the same relative percentage-point change to all 256 slots.
    /// Each slot clamps independently, preserving accents until saturation.
    pub fn adjust_all(&mut self, delta: i32) -> bool {
        let mut changed = false;
        let mut sum = 0_u32;
        for percent in &mut self.percents {
            let adjusted = adjust_volume_percent(*percent, delta);
            changed |= adjusted != *percent;
            *percent = adjusted;
            sum += u32::from(adjusted);
        }
        self.sum = sum;
        changed
    }
}

impl Default for TriggerVolumes {
    fn default() -> Self {
        Self::all_default()
    }
}

pub fn pattern_step_index(step: u16, division: u16) -> Option<usize> {
    if division == 0 || division > MAX_BEAT_MULTIPLIER || step >= division {
        return None;
    }
    Some(usize::from(step))
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

/// Stable index into the firmware's fixed sample catalog.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct SampleId(u8);

impl SampleId {
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < SAMPLE_COUNT {
            Some(Self(index as u8))
        } else {
            None
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub fn clamped_offset(self, delta: i32) -> Self {
        let index = (self.index() as i64)
            .saturating_add(i64::from(delta))
            .clamp(0, (SAMPLE_COUNT - 1) as i64);
        Self(index as u8)
    }
}

/// Apply one physical sample-selector detent without encoder acceleration,
/// clamping at the catalog endpoints.
pub fn adjust_sample_selection(current: SampleId, direction: i32) -> SampleId {
    current.clamped_offset(direction.signum())
}

/// One immutable entry in the fixed firmware sample bank.
#[derive(Clone, Copy, Debug)]
pub struct SampleDefinition<'a> {
    pub id: SampleId,
    pub short_name: &'static str,
    pub pcm: WavPcm16<'a>,
}

/// Validated PCM data and display names for all stable sample IDs.
#[derive(Clone, Copy, Debug)]
pub struct SampleCatalog<'a> {
    samples: [WavPcm16<'a>; SAMPLE_COUNT],
    short_names: &'static [&'static str; SAMPLE_COUNT],
}

impl<'a> SampleCatalog<'a> {
    pub const fn new(
        samples: [WavPcm16<'a>; SAMPLE_COUNT],
        short_names: &'static [&'static str; SAMPLE_COUNT],
    ) -> Self {
        Self {
            samples,
            short_names,
        }
    }

    pub const fn samples(&self) -> &[WavPcm16<'a>; SAMPLE_COUNT] {
        &self.samples
    }

    pub fn definition(&self, id: SampleId) -> SampleDefinition<'a> {
        SampleDefinition {
            id,
            short_name: self.short_names[id.index()],
            pcm: self.samples[id.index()],
        }
    }

    #[inline]
    fn pcm(&self, id: SampleId) -> WavPcm16<'a> {
        self.samples[id.index()]
    }
}

pub const DEFAULT_KICK_SAMPLE: SampleId = SampleId(DEFAULT_KICK_INDEX as u8);
pub const DEFAULT_OPEN_HAT_SAMPLE: SampleId = SampleId(DEFAULT_OPEN_HAT_INDEX as u8);
pub const DEFAULT_PAD_SAMPLES: [SampleId; BEAT_PAD_COUNT] = [
    DEFAULT_KICK_SAMPLE,
    DEFAULT_KICK_SAMPLE,
    DEFAULT_KICK_SAMPLE,
    DEFAULT_KICK_SAMPLE,
    DEFAULT_KICK_SAMPLE,
    DEFAULT_KICK_SAMPLE,
    DEFAULT_OPEN_HAT_SAMPLE,
    DEFAULT_OPEN_HAT_SAMPLE,
    DEFAULT_OPEN_HAT_SAMPLE,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreviewRequest {
    pub pad: usize,
    pub sample: SampleId,
}

impl PreviewRequest {
    pub const fn new(pad: usize, sample: SampleId) -> Option<Self> {
        if pad < BEAT_PAD_COUNT {
            Some(Self { pad, sample })
        } else {
            None
        }
    }
}

/// Build the automatic preview for an actual Sample-mode assignment change.
/// Holding the encoder button makes push-and-turn browsing silent, and an
/// outward detent at a clamped endpoint does not replay the unchanged sample.
pub const fn sample_selection_preview_request(
    pad: usize,
    previous_sample: SampleId,
    selected_sample: SampleId,
    encoder_button_held: bool,
) -> Option<PreviewRequest> {
    if encoder_button_held || selected_sample.index() == previous_sample.index() {
        None
    } else {
        PreviewRequest::new(pad, selected_sample)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PadState {
    pub beats_per_interval: u16,
    pub tick_ordinal: u128,
    pub next_frame: Option<u64>,
    pub sample: SampleId,
    // `next_frame` is the exact ceil-scheduled deadline for `tick_ordinal`.
    // These private fields advance that deadline as a rational accumulator,
    // avoiding wide division and modulo in the audio-frame hot path.
    period_numerator: u64,
    period_denominator: u32,
    whole_frames: u64,
    period_remainder: u32,
    deadline_error: u32,
    next_step: u16,
}

impl PadState {
    const fn new(pad: usize) -> Self {
        Self {
            beats_per_interval: 0,
            tick_ordinal: 0,
            next_frame: None,
            sample: DEFAULT_PAD_SAMPLES[pad],
            period_numerator: 0,
            period_denominator: 0,
            whole_frames: 0,
            period_remainder: 0,
            deadline_error: 0,
            next_step: 0,
        }
    }

    fn disable_clock(&mut self) {
        self.tick_ordinal = 0;
        self.next_frame = None;
        self.period_numerator = 0;
        self.period_denominator = 0;
        self.whole_frames = 0;
        self.period_remainder = 0;
        self.deadline_error = 0;
        self.next_step = 0;
    }

    /// Align this pad to the first global-grid tick strictly after `from_frame`.
    ///
    /// Wide arithmetic is intentionally confined to timing changes. Once
    /// aligned, normal rendering advances deadlines with additions and
    /// comparisons only.
    fn align_clock(&mut self, beats_per_interval: u16, base_interval_ms: u32, from_frame: u64) {
        debug_assert!(beats_per_interval != 0);

        let period_numerator = u64::from(SAMPLE_RATE) * u64::from(base_interval_ms);
        let period_denominator = 1_000_u32 * u32::from(beats_per_interval);
        let denominator = u128::from(period_denominator);
        let ordinal = (u128::from(from_frame) * denominator) / u128::from(period_numerator) + 1;
        let tick_numerator = ordinal * u128::from(period_numerator);
        let deadline = tick_numerator.div_ceil(denominator);
        let deadline_error = deadline * denominator - tick_numerator;

        self.tick_ordinal = ordinal;
        self.next_frame = Some(deadline as u64);
        self.period_numerator = period_numerator;
        self.period_denominator = period_denominator;
        self.whole_frames = period_numerator / u64::from(period_denominator);
        self.period_remainder = (period_numerator % u64::from(period_denominator)) as u32;
        self.deadline_error = deadline_error as u32;
        self.next_step = ((ordinal - 1) % u128::from(beats_per_interval)) as u16;
    }

    /// Consume every tick due at `frame`, coalescing enabled triggers at the
    /// loudest stored level. `Some(0)` remains distinct from no enabled tick.
    fn take_due_trigger(
        &mut self,
        frame: u64,
        pattern: &Pattern,
        trigger_volumes: &TriggerVolumes,
    ) -> Option<u8> {
        let next_frame = self.next_frame?;
        if !frame_has_reached(frame, next_frame) {
            return None;
        }

        let mut trigger_volume = None;

        // In contiguous rendering, deadlines equal the current frame. The
        // current 256-step limit leaves multiple frames between deadlines;
        // the second iteration preserves a bounded path if that limit grows.
        if next_frame == frame {
            for _ in 0..2 {
                if self.next_frame != Some(frame) {
                    break;
                }
                trigger_volume = max_trigger_volume(
                    trigger_volume,
                    self.current_step_trigger_volume(pattern, trigger_volumes),
                );
                self.advance_one();
            }

            // Retain an exact bounded recovery path if constraints are ever
            // widened enough for more than two ticks to share one frame.
            if self
                .next_frame
                .is_some_and(|deadline| frame_has_reached(frame, deadline))
            {
                trigger_volume = max_trigger_volume(
                    trigger_volume,
                    self.take_due_fast_trigger(frame, pattern, trigger_volumes),
                );
            }
            return trigger_volume;
        }

        // A non-contiguous render coalesces the entire missed range exactly,
        // without replaying an unbounded number of logical ticks.
        self.take_due_fast_trigger(frame, pattern, trigger_volumes)
    }

    #[cfg(test)]
    fn take_due(&mut self, frame: u64, pattern: &Pattern) -> bool {
        self.take_due_trigger(frame, pattern, &TriggerVolumes::all_default())
            .is_some()
    }

    fn current_step_trigger_volume(
        &self,
        pattern: &Pattern,
        trigger_volumes: &TriggerVolumes,
    ) -> Option<u8> {
        let step = usize::from(self.next_step);
        pattern
            .bit(step)
            .unwrap_or(false)
            .then(|| trigger_volumes.percent(step).unwrap_or(0))
    }

    #[cfg(test)]
    fn current_step_enabled(&self, pattern: &Pattern) -> bool {
        pattern.bit(usize::from(self.next_step)).unwrap_or(false)
    }

    fn advance_one(&mut self) {
        let deadline = self
            .next_frame
            .expect("an enabled pad always has a deadline");
        let carry = self.period_remainder > self.deadline_error;
        let frame_delta = self.whole_frames + u64::from(carry);

        self.deadline_error = if carry {
            self.deadline_error + self.period_denominator - self.period_remainder
        } else {
            self.deadline_error - self.period_remainder
        };
        self.next_frame = Some(deadline.wrapping_add(frame_delta));
        self.tick_ordinal = self.tick_ordinal.wrapping_add(1);
        self.next_step += 1;
        if self.next_step == self.beats_per_interval {
            self.next_step = 0;
        }
    }

    fn take_due_fast_trigger(
        &mut self,
        frame: u64,
        pattern: &Pattern,
        trigger_volumes: &TriggerVolumes,
    ) -> Option<u8> {
        let deadline = self
            .next_frame
            .expect("an enabled pad always has a deadline");
        let lag = frame.wrapping_sub(deadline);
        debug_assert!(lag < (1_u64 << 63));

        // If e = deadline * B - ordinal * A, then the number of additional
        // deadlines through deadline + lag is floor((e + lag * B) / A).
        let due = (u128::from(self.deadline_error)
            + u128::from(lag) * u128::from(self.period_denominator))
            / u128::from(self.period_numerator)
            + 1;
        let trigger_volume = self.max_enabled_step_volume(pattern, trigger_volumes, due);
        self.advance_many(due);
        trigger_volume
    }

    fn max_enabled_step_volume(
        &self,
        pattern: &Pattern,
        trigger_volumes: &TriggerVolumes,
        due: u128,
    ) -> Option<u8> {
        let division = self.beats_per_interval;
        let unique_steps = if due >= u128::from(division) {
            division
        } else {
            due as u16
        };

        let mut step = self.next_step;
        let mut trigger_volume = None;
        for _ in 0..unique_steps {
            if pattern.bit(usize::from(step)).unwrap_or(false) {
                trigger_volume =
                    max_trigger_volume(trigger_volume, trigger_volumes.percent(usize::from(step)));
                if trigger_volume == Some(DEFAULT_TRIGGER_VOLUME_PERCENT) {
                    return trigger_volume;
                }
            }
            step += 1;
            if step == division {
                step = 0;
            }
        }
        trigger_volume
    }

    fn advance_many(&mut self, due: u128) {
        debug_assert!(due != 0);
        let denominator = u128::from(self.period_denominator);
        let elapsed_numerator = due * u128::from(self.period_numerator);
        let rounding_bias = denominator - 1 - u128::from(self.deadline_error);
        let frame_delta = (elapsed_numerator + rounding_bias) / denominator;
        let new_error =
            u128::from(self.deadline_error) + frame_delta * denominator - elapsed_numerator;

        let deadline = self
            .next_frame
            .expect("an enabled pad always has a deadline");
        self.next_frame = Some(deadline.wrapping_add(frame_delta as u64));
        self.deadline_error = new_error as u32;
        self.tick_ordinal = self.tick_ordinal.wrapping_add(due);

        let division = u128::from(self.beats_per_interval);
        let step_advance = (due % division) as u16;
        let next_step = u32::from(self.next_step) + u32::from(step_advance);
        self.next_step = (next_step % u32::from(self.beats_per_interval)) as u16;
    }
}

const fn max_trigger_volume(current: Option<u8>, candidate: Option<u8>) -> Option<u8> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(if current > candidate {
            current
        } else {
            candidate
        }),
        (Some(current), None) => Some(current),
        (None, Some(candidate)) => Some(candidate),
        (None, None) => None,
    }
}

const UNIT_Q16: u32 = 65_536;

const fn trigger_gain_lut() -> [u32; DEFAULT_TRIGGER_VOLUME_PERCENT as usize + 1] {
    let mut gains = [0_u32; DEFAULT_TRIGGER_VOLUME_PERCENT as usize + 1];
    let mut percent = 0;
    while percent <= DEFAULT_TRIGGER_VOLUME_PERCENT as usize {
        gains[percent] = percent as u32 * UNIT_Q16 / 100;
        percent += 1;
    }
    gains
}

const TRIGGER_GAIN_Q16: [u32; DEFAULT_TRIGGER_VOLUME_PERCENT as usize + 1] = trigger_gain_lut();

/// Captured gain for one scheduled pattern trigger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggerGain(u8);

impl TriggerGain {
    pub const ZERO: Self = Self(0);
    pub const FULL: Self = Self(DEFAULT_TRIGGER_VOLUME_PERCENT);

    pub const fn from_percent(percent: u8) -> Self {
        Self(if percent > DEFAULT_TRIGGER_VOLUME_PERCENT {
            DEFAULT_TRIGGER_VOLUME_PERCENT
        } else {
            percent
        })
    }

    pub const fn percent(self) -> u8 {
        self.0
    }

    const fn q16(self) -> u32 {
        match self.0 {
            0 => 0,
            DEFAULT_TRIGGER_VOLUME_PERCENT => UNIT_Q16,
            percent => TRIGGER_GAIN_Q16[percent as usize],
        }
    }
}

#[inline]
fn multiply_unit_q16(live_q16: u32, captured_q16: u32) -> u32 {
    debug_assert!(live_q16 <= UNIT_Q16 && captured_q16 <= UNIT_Q16);
    match (live_q16, captured_q16) {
        (0, _) | (_, 0) => 0,
        (UNIT_Q16, captured) => captured,
        (live, UNIT_Q16) => live,
        (live, captured) => (live * captured) >> 16,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VoicePhase {
    Idle,
    Playing,
    Releasing(u8),
}

#[derive(Clone, Copy, Debug)]
struct PlaybackVoice {
    phase: VoicePhase,
    owner_pad: u8,
    sample: SampleId,
    trigger_gain_q16: u32,
    cursor: usize,
    started_serial: u64,
}

impl PlaybackVoice {
    const fn idle() -> Self {
        Self {
            phase: VoicePhase::Idle,
            owner_pad: 0,
            sample: SampleId(0),
            trigger_gain_q16: UNIT_Q16,
            cursor: 0,
            started_serial: 0,
        }
    }

    const fn is_active(&self) -> bool {
        !matches!(self.phase, VoicePhase::Idle)
    }

    const fn owner_pad(&self) -> usize {
        self.owner_pad as usize
    }

    #[cfg(test)]
    fn start(&mut self, pad: usize, sample: SampleId, serial: u64) {
        self.start_with_trigger_gain(pad, sample, TriggerGain::FULL, serial);
    }

    fn start_with_trigger_gain(
        &mut self,
        pad: usize,
        sample: SampleId,
        trigger_gain: TriggerGain,
        serial: u64,
    ) {
        self.phase = VoicePhase::Playing;
        self.owner_pad = pad as u8;
        self.sample = sample;
        self.trigger_gain_q16 = trigger_gain.q16();
        self.cursor = 0;
        self.started_serial = serial;
    }

    fn force_release(&mut self) -> bool {
        if self.phase != VoicePhase::Playing {
            return false;
        }
        self.phase = VoicePhase::Releasing(DECLICK_FRAMES);
        true
    }

    const fn fade_frames(&self) -> u8 {
        match self.phase {
            VoicePhase::Releasing(remaining) => remaining,
            VoicePhase::Idle | VoicePhase::Playing => DECLICK_FRAMES,
        }
    }

    fn render(&mut self, catalog: &SampleCatalog<'_>, gain_q16: u32) -> i32 {
        if !self.is_active() {
            return 0;
        }

        let pcm = catalog.pcm(self.sample);
        let Some(raw) = pcm.sample(self.cursor) else {
            self.phase = VoicePhase::Idle;
            return 0;
        };
        self.cursor += 1;

        let effective_gain_q16 = multiply_unit_q16(gain_q16, self.trigger_gain_q16);
        let mut contribution = apply_sample_gain_q16(raw, effective_gain_q16);
        if let VoicePhase::Releasing(remaining) = self.phase {
            contribution = scale_declick(contribution, remaining);
            if remaining <= 1 {
                self.phase = VoicePhase::Idle;
            } else {
                self.phase = VoicePhase::Releasing(remaining - 1);
            }
        }

        if self.cursor >= pcm.len() {
            self.phase = VoicePhase::Idle;
        }
        contribution
    }
}

#[derive(Clone, Copy, Debug)]
struct FadeTail {
    active: bool,
    owner_pad: u8,
    sample: SampleId,
    trigger_gain_q16: u32,
    cursor: usize,
    remaining: u8,
    started_serial: u64,
}

impl FadeTail {
    const fn idle() -> Self {
        Self {
            active: false,
            owner_pad: 0,
            sample: SampleId(0),
            trigger_gain_q16: UNIT_Q16,
            cursor: 0,
            remaining: 0,
            started_serial: 0,
        }
    }

    fn start_from(&mut self, voice: PlaybackVoice) {
        self.active = true;
        self.owner_pad = voice.owner_pad;
        self.sample = voice.sample;
        self.trigger_gain_q16 = voice.trigger_gain_q16;
        self.cursor = voice.cursor;
        self.remaining = voice.fade_frames();
        self.started_serial = voice.started_serial;
    }

    fn render(&mut self, catalog: &SampleCatalog<'_>, gain_q16: u32) -> i32 {
        if !self.active {
            return 0;
        }

        let pcm = catalog.pcm(self.sample);
        let Some(raw) = pcm.sample(self.cursor) else {
            self.active = false;
            return 0;
        };
        self.cursor += 1;
        let effective_gain_q16 = multiply_unit_q16(gain_q16, self.trigger_gain_q16);
        let contribution = scale_declick(
            apply_sample_gain_q16(raw, effective_gain_q16),
            self.remaining,
        );
        if self.remaining <= 1 || self.cursor >= pcm.len() {
            self.active = false;
        } else {
            self.remaining -= 1;
        }
        contribution
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartPriority {
    Scheduled,
    Preview,
}

#[derive(Clone, Copy, Debug)]
struct VoiceStart {
    sample: SampleId,
    trigger_gain: TriggerGain,
}

impl VoiceStart {
    const fn full(sample: SampleId) -> Self {
        Self {
            sample,
            trigger_gain: TriggerGain::FULL,
        }
    }

    const fn with_trigger_gain(sample: SampleId, trigger_gain: TriggerGain) -> Self {
        Self {
            sample,
            trigger_gain,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct GainRamp {
    start_q16: u32,
    current_q16: u32,
    target_q16: u32,
    target_percent: u8,
    elapsed: u8,
}

impl GainRamp {
    const fn new(percent: u8) -> Self {
        let q16 = percent_to_q16(percent);
        Self {
            start_q16: q16,
            current_q16: q16,
            target_q16: q16,
            target_percent: percent,
            elapsed: GAIN_RAMP_FRAMES,
        }
    }

    fn set_percent(&mut self, percent: u8) {
        let percent = percent.min(100);
        if percent == self.target_percent {
            return;
        }
        self.start_q16 = self.current_q16;
        self.target_q16 = percent_to_q16(percent);
        self.target_percent = percent;
        self.elapsed = 0;
    }

    fn next_q16(&mut self) -> u32 {
        if self.elapsed >= GAIN_RAMP_FRAMES {
            return self.current_q16;
        }
        self.elapsed += 1;
        let start = self.start_q16 as i32;
        let delta = self.target_q16 as i32 - start;
        let interpolated = trunc_div_pow2_i32(delta * i32::from(self.elapsed), GAIN_RAMP_SHIFT);
        self.current_q16 = (start + interpolated) as u32;
        self.current_q16
    }

    const fn target_percent(&self) -> u8 {
        self.target_percent
    }

    const fn current_q16(&self) -> u32 {
        self.current_q16
    }
}

const fn percent_to_q16(percent: u8) -> u32 {
    (percent as u32 * 65_536) / 100
}

#[inline]
fn trunc_div_pow2_i32(value: i32, shift: u32) -> i32 {
    debug_assert!(shift > 0 && shift < i32::BITS);
    let truncated_down = value >> shift;
    let discarded_mask = (1_i32 << shift) - 1;
    truncated_down + i32::from(value < 0 && value & discarded_mask != 0)
}

#[inline]
fn apply_sample_gain_q16(sample: i16, gain_q16: u32) -> i32 {
    debug_assert!(gain_q16 <= 65_536);
    match gain_q16 {
        0 => 0,
        65_536 => i32::from(sample),
        _ => trunc_div_pow2_i32(i32::from(sample) * gain_q16 as i32, 16),
    }
}

#[inline]
fn scale_declick(value: i32, remaining: u8) -> i32 {
    debug_assert!(remaining <= DECLICK_FRAMES);
    // `value` is a gain-scaled i16 and `remaining` is at most 32, so the
    // product is bounded by +/-1,048,576 and cannot overflow an i32.
    trunc_div_pow2_i32(value * i32::from(remaining), DECLICK_SHIFT)
}

#[inline]
fn apply_mix_gain_q16(value: i32, gain_q16: u32) -> i32 {
    debug_assert!(gain_q16 <= 65_536);
    match gain_q16 {
        0 => 0,
        65_536 => value,
        _ => {
            // Split the magnitude at the Q16 radix so both products fit u32:
            // `(2^15 * 65_535)` and `(65_535 * 65_535)` are each below
            // `u32::MAX`. Applying the sign last preserves division's
            // truncation toward zero, including for `i32::MIN`.
            let magnitude = value.unsigned_abs();
            let whole = (magnitude >> 16) * gain_q16;
            let fraction = ((magnitude & 0xffff) * gain_q16) >> 16;
            let scaled = (whole + fraction) as i32;
            if value < 0 { -scaled } else { scaled }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct VoiceAllocationState {
    target_silent_mask: u16,
    current_silent_mask: u16,
}

impl VoiceAllocationState {
    fn new(
        master_target_percent: u8,
        pad_target_percents: &[u8; BEAT_PAD_COUNT],
        master_current_q16: u32,
        pad_current_q16: &[u32; BEAT_PAD_COUNT],
    ) -> Self {
        let target_silent_mask = if master_target_percent == 0 {
            BEAT_PAD_MASK
        } else {
            pad_target_percents
                .iter()
                .enumerate()
                .fold(0_u16, |mask, (pad, volume)| {
                    mask | (u16::from(*volume == 0) << pad)
                })
        };
        let current_silent_mask = if master_current_q16 == 0 {
            BEAT_PAD_MASK
        } else {
            pad_current_q16
                .iter()
                .enumerate()
                .fold(0_u16, |mask, (pad, gain)| {
                    mask | (u16::from(*gain == 0) << pad)
                })
        };
        Self {
            target_silent_mask,
            current_silent_mask,
        }
    }

    #[cfg(test)]
    fn settled(master_percent: u8, pad_percents: &[u8; BEAT_PAD_COUNT]) -> Self {
        let master_q16 = percent_to_q16(master_percent);
        let pad_q16 = core::array::from_fn(|pad| percent_to_q16(pad_percents[pad]));
        Self::new(master_percent, pad_percents, master_q16, &pad_q16)
    }

    const fn target_is_silent(self, pad: usize) -> bool {
        self.target_silent_mask & (1_u16 << pad) != 0
    }

    const fn current_is_silent(self, pad: usize) -> bool {
        self.current_silent_mask & (1_u16 << pad) != 0
    }
}

#[derive(Clone, Copy, Debug)]
struct VoicePool {
    primaries: [PlaybackVoice; PRIMARY_VOICE_COUNT],
    tails: [FadeTail; FADE_TAIL_COUNT],
    next_serial: u64,
    active_primary_count: u8,
    active_tail_count: u8,
}

impl VoicePool {
    const fn new() -> Self {
        Self {
            primaries: [PlaybackVoice::idle(); PRIMARY_VOICE_COUNT],
            tails: [FadeTail::idle(); FADE_TAIL_COUNT],
            next_serial: 0,
            active_primary_count: 0,
            active_tail_count: 0,
        }
    }

    fn active_voice_count(&self) -> usize {
        usize::from(self.active_primary_count)
    }

    fn active_voice_count_for_pad(&self, pad: usize) -> usize {
        self.primaries
            .iter()
            .filter(|voice| voice.is_active() && voice.owner_pad() == pad)
            .count()
    }

    fn active_tail_count(&self) -> usize {
        usize::from(self.active_tail_count)
    }

    fn oldest_primary_matching(
        &self,
        mut predicate: impl FnMut(&PlaybackVoice) -> bool,
    ) -> Option<usize> {
        let mut selected = None;
        let mut greatest_age = 0_u64;
        for (index, voice) in self.primaries.iter().enumerate() {
            if !voice.is_active() || !predicate(voice) {
                continue;
            }
            let age = self.next_serial.wrapping_sub(voice.started_serial);
            if selected.is_none() || age > greatest_age {
                selected = Some(index);
                greatest_age = age;
            }
        }
        selected
    }

    fn preserve_stolen_voice(
        &mut self,
        voice: PlaybackVoice,
        max_fade_tails: u8,
        report: &mut RenderReport,
    ) {
        if max_fade_tails == 0 {
            report.load_shed_fade_tail_count = report.load_shed_fade_tail_count.saturating_add(1);
            return;
        }
        let tail_limit = max_fade_tails.min(FADE_TAIL_COUNT as u8);
        let free = (self.active_tail_count < tail_limit)
            .then(|| self.tails.iter().position(|tail| !tail.active))
            .flatten();
        let index = free.unwrap_or_else(|| {
            report.fade_tail_overflow_count = report.fade_tail_overflow_count.saturating_add(1);
            let mut selected = self.tails.iter().position(|tail| tail.active).unwrap_or(0);
            for candidate in selected + 1..FADE_TAIL_COUNT {
                let current = &self.tails[selected];
                let other = &self.tails[candidate];
                if !other.active {
                    continue;
                }
                let current_age = self.next_serial.wrapping_sub(current.started_serial);
                let other_age = self.next_serial.wrapping_sub(other.started_serial);
                if other.remaining < current.remaining
                    || (other.remaining == current.remaining
                        && (other_age > current_age
                            || (other_age == current_age && candidate < selected)))
                {
                    selected = candidate;
                }
            }
            selected
        });
        let was_active = self.tails[index].active;
        self.tails[index].start_from(voice);
        if !was_active {
            self.active_tail_count += 1;
            debug_assert!(usize::from(self.active_tail_count) <= FADE_TAIL_COUNT);
        }
    }

    #[cfg(test)]
    fn start(
        &mut self,
        pad: usize,
        sample: SampleId,
        priority: StartPriority,
        allocation: VoiceAllocationState,
        report: &mut RenderReport,
    ) -> bool {
        self.start_with_policy(
            pad,
            sample,
            priority,
            allocation,
            RenderPolicy::FULL,
            report,
        )
    }

    #[cfg_attr(target_arch = "arm", unsafe(link_section = ".data.ram_func"))]
    #[inline(never)]
    fn start_with_policy(
        &mut self,
        pad: usize,
        sample: SampleId,
        priority: StartPriority,
        allocation: VoiceAllocationState,
        policy: RenderPolicy,
        report: &mut RenderReport,
    ) -> bool {
        self.start_with_policy_and_trigger_gain(
            pad,
            VoiceStart::full(sample),
            priority,
            allocation,
            policy,
            report,
        )
    }

    #[cfg_attr(target_arch = "arm", unsafe(link_section = ".data.ram_func"))]
    #[inline(never)]
    fn start_with_policy_and_trigger_gain(
        &mut self,
        pad: usize,
        start: VoiceStart,
        priority: StartPriority,
        allocation: VoiceAllocationState,
        policy: RenderPolicy,
        report: &mut RenderReport,
    ) -> bool {
        let primary_limit = policy
            .max_primary_voices
            .clamp(1, PRIMARY_VOICE_COUNT as u8);
        // A pressure transition may have placed excess voices into their
        // short in-place release. Do not immediately reuse one and defeat the
        // contraction; admit new work again after the releases finish.
        if self.active_primary_count > primary_limit {
            match priority {
                StartPriority::Scheduled => {
                    report.load_shed_trigger_count =
                        report.load_shed_trigger_count.saturating_add(1);
                }
                StartPriority::Preview => {
                    report.load_shed_preview_count =
                        report.load_shed_preview_count.saturating_add(1);
                }
            }
            return false;
        }
        let mut victim = if self.active_primary_count < primary_limit {
            self.primaries.iter().position(|voice| !voice.is_active())
        } else {
            None
        };
        let mut steal_kind = None;
        let silent_scheduled_request =
            priority == StartPriority::Scheduled && allocation.target_is_silent(pad);

        if victim.is_none() && (!silent_scheduled_request || allocation.current_is_silent(pad)) {
            victim = self.oldest_primary_matching(|voice| voice.owner_pad() == pad);
            if victim.is_some() {
                steal_kind = Some(0_u8);
            }
        }

        if victim.is_none() && priority == StartPriority::Scheduled {
            victim = self.oldest_primary_matching(|voice| {
                allocation.target_is_silent(voice.owner_pad())
                    && (!silent_scheduled_request
                        || allocation.current_is_silent(voice.owner_pad()))
            });
            if victim.is_some() {
                steal_kind = Some(1_u8);
            }
        }

        if victim.is_none() && priority == StartPriority::Scheduled {
            if silent_scheduled_request {
                report.silent_trigger_drop_count =
                    report.silent_trigger_drop_count.saturating_add(1);
                return false;
            }
            victim = self.oldest_primary_matching(|_| true);
            if victim.is_some() {
                steal_kind = Some(2_u8);
            }
        }

        let Some(index) = victim else {
            report.preview_drop_count = report.preview_drop_count.saturating_add(1);
            return false;
        };

        if let Some(kind) = steal_kind {
            let stolen = self.primaries[index];
            let tail_limit = if policy.preserve_stolen_fade_tails {
                policy.max_fade_tails
            } else {
                0
            };
            self.preserve_stolen_voice(stolen, tail_limit, report);
            match kind {
                0 => report.same_pad_steal_count = report.same_pad_steal_count.saturating_add(1),
                1 => {
                    report.zero_volume_steal_count =
                        report.zero_volume_steal_count.saturating_add(1)
                }
                _ => report.global_steal_count = report.global_steal_count.saturating_add(1),
            }
        }

        let was_active = self.primaries[index].is_active();
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        self.primaries[index].start_with_trigger_gain(
            pad,
            start.sample,
            start.trigger_gain,
            serial,
        );
        if !was_active {
            self.active_primary_count += 1;
            debug_assert!(usize::from(self.active_primary_count) <= PRIMARY_VOICE_COUNT);
        }
        true
    }

    fn enforce_policy(&mut self, policy: RenderPolicy, report: &mut RenderReport) {
        let primary_limit = policy
            .max_primary_voices
            .clamp(1, PRIMARY_VOICE_COUNT as u8);
        if policy.release_excess_primaries && self.active_primary_count > primary_limit {
            let excess = self.active_primary_count - primary_limit;
            let already_releasing = self
                .primaries
                .iter()
                .filter(|voice| matches!(voice.phase, VoicePhase::Releasing(_)))
                .count()
                .min(usize::from(u8::MAX)) as u8;
            let mut to_release = excess.saturating_sub(already_releasing);
            while to_release != 0 {
                let victim =
                    self.oldest_primary_matching(|voice| voice.phase == VoicePhase::Playing);
                let Some(index) = victim else {
                    break;
                };
                if self.primaries[index].force_release() {
                    report.load_shed_primary_count =
                        report.load_shed_primary_count.saturating_add(1);
                    to_release -= 1;
                }
            }
        }
        while policy.trim_excess_primaries && self.active_primary_count > primary_limit {
            let victim = self
                .oldest_primary_matching(|voice| matches!(voice.phase, VoicePhase::Releasing(_)))
                .or_else(|| self.oldest_primary_matching(|_| true));
            let Some(index) = victim else {
                break;
            };
            self.primaries[index].phase = VoicePhase::Idle;
            self.active_primary_count -= 1;
            report.load_shed_primary_count = report.load_shed_primary_count.saturating_add(1);
        }

        let tail_limit = policy.max_fade_tails.min(FADE_TAIL_COUNT as u8);
        while self.active_tail_count > tail_limit {
            let mut victim = None;
            for (index, tail) in self.tails.iter().enumerate() {
                if !tail.active {
                    continue;
                }
                let Some(current) = victim else {
                    victim = Some(index);
                    continue;
                };
                if tail.remaining < self.tails[current].remaining
                    || (tail.remaining == self.tails[current].remaining
                        && tail.started_serial < self.tails[current].started_serial)
                {
                    victim = Some(index);
                }
            }
            let Some(index) = victim else {
                break;
            };
            self.tails[index].active = false;
            self.active_tail_count -= 1;
            report.load_shed_fade_tail_count = report.load_shed_fade_tail_count.saturating_add(1);
        }
    }

    fn release_mask(&mut self, mask: u16) -> u16 {
        let mut released = 0_u16;
        for voice in &mut self.primaries {
            if voice.is_active()
                && mask & (1_u16 << voice.owner_pad()) != 0
                && voice.force_release()
            {
                released = released.saturating_add(1);
            }
        }
        released
    }

    fn render(
        &mut self,
        catalog: &SampleCatalog<'_>,
        pad_gains_q16: &[u32; BEAT_PAD_COUNT],
    ) -> i32 {
        let mut total = 0_i32;
        let mut ended_primaries = 0_u8;
        for voice in &mut self.primaries {
            if !voice.is_active() {
                continue;
            }
            let gain = pad_gains_q16[voice.owner_pad()];
            total += voice.render(catalog, gain);
            ended_primaries += u8::from(!voice.is_active());
        }
        debug_assert!(ended_primaries <= self.active_primary_count);
        self.active_primary_count -= ended_primaries;

        let mut ended_tails = 0_u8;
        for tail in &mut self.tails {
            if !tail.active {
                continue;
            }
            let gain = pad_gains_q16[tail.owner_pad as usize];
            total += tail.render(catalog, gain);
            ended_tails += u8::from(!tail.active);
        }
        debug_assert!(ended_tails <= self.active_tail_count);
        self.active_tail_count -= ended_tails;

        // At most 24 primaries and nine tails can each contribute one
        // gain-bounded i16 sample, so the accumulator is within
        // -1,081,344..=1,081,311 and cannot overflow i32.
        total
    }
}

/// Per-block events and bounded-pool diagnostics produced while rendering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RenderReport {
    pub latest_visual_triggers: [Option<u64>; BEAT_PAD_COUNT],
    pub audible_trigger_counts: [u16; SAMPLE_COUNT],
    pub scheduled_voice_start_count: u16,
    pub preview_voice_start_count: u16,
    pub same_pad_steal_count: u16,
    pub zero_volume_steal_count: u16,
    pub global_steal_count: u16,
    pub silent_trigger_drop_count: u16,
    pub preview_drop_count: u16,
    pub fade_tail_overflow_count: u16,
    pub muted_voice_release_count: u16,
    pub clipped_frame_count: u16,
    pub load_shed_preview_count: u16,
    pub load_shed_fade_tail_count: u16,
    pub load_shed_trigger_count: u16,
    pub load_shed_primary_count: u16,
    pub coarse_dither_frame_count: u16,
    pub peak_primary_voice_count: u8,
    pub peak_fade_tail_count: u8,
    pub peak_total_voice_count: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SamplerDiagnostics {
    pub scheduled_voice_start_count: u32,
    pub preview_voice_start_count: u32,
    pub same_pad_steal_count: u32,
    pub zero_volume_steal_count: u32,
    pub global_steal_count: u32,
    pub silent_trigger_drop_count: u32,
    pub preview_drop_count: u32,
    pub fade_tail_overflow_count: u32,
    pub muted_voice_release_count: u32,
    pub clipped_frame_count: u32,
    pub load_shed_preview_count: u32,
    pub load_shed_fade_tail_count: u32,
    pub load_shed_trigger_count: u32,
    pub load_shed_primary_count: u32,
    pub coarse_dither_frame_count: u32,
    pub peak_primary_voice_count: u8,
    pub peak_fade_tail_count: u8,
    pub peak_total_voice_count: u8,
}

impl SamplerDiagnostics {
    pub fn record(&mut self, report: &RenderReport) {
        self.scheduled_voice_start_count = self
            .scheduled_voice_start_count
            .saturating_add(u32::from(report.scheduled_voice_start_count));
        self.preview_voice_start_count = self
            .preview_voice_start_count
            .saturating_add(u32::from(report.preview_voice_start_count));
        self.same_pad_steal_count = self
            .same_pad_steal_count
            .saturating_add(u32::from(report.same_pad_steal_count));
        self.zero_volume_steal_count = self
            .zero_volume_steal_count
            .saturating_add(u32::from(report.zero_volume_steal_count));
        self.global_steal_count = self
            .global_steal_count
            .saturating_add(u32::from(report.global_steal_count));
        self.silent_trigger_drop_count = self
            .silent_trigger_drop_count
            .saturating_add(u32::from(report.silent_trigger_drop_count));
        self.preview_drop_count = self
            .preview_drop_count
            .saturating_add(u32::from(report.preview_drop_count));
        self.fade_tail_overflow_count = self
            .fade_tail_overflow_count
            .saturating_add(u32::from(report.fade_tail_overflow_count));
        self.muted_voice_release_count = self
            .muted_voice_release_count
            .saturating_add(u32::from(report.muted_voice_release_count));
        self.clipped_frame_count = self
            .clipped_frame_count
            .saturating_add(u32::from(report.clipped_frame_count));
        self.load_shed_preview_count = self
            .load_shed_preview_count
            .saturating_add(u32::from(report.load_shed_preview_count));
        self.load_shed_fade_tail_count = self
            .load_shed_fade_tail_count
            .saturating_add(u32::from(report.load_shed_fade_tail_count));
        self.load_shed_trigger_count = self
            .load_shed_trigger_count
            .saturating_add(u32::from(report.load_shed_trigger_count));
        self.load_shed_primary_count = self
            .load_shed_primary_count
            .saturating_add(u32::from(report.load_shed_primary_count));
        self.coarse_dither_frame_count = self
            .coarse_dither_frame_count
            .saturating_add(u32::from(report.coarse_dither_frame_count));
        self.peak_primary_voice_count = self
            .peak_primary_voice_count
            .max(report.peak_primary_voice_count);
        self.peak_fade_tail_count = self.peak_fade_tail_count.max(report.peak_fade_tail_count);
        self.peak_total_voice_count = self
            .peak_total_voice_count
            .max(report.peak_total_voice_count);
    }
}

fn record_voice_start(
    report: &mut RenderReport,
    sample: SampleId,
    trigger_gain: TriggerGain,
    pad_volume_percent: u8,
    master_volume_percent: u8,
    sample_is_empty: bool,
    priority: StartPriority,
) {
    match priority {
        StartPriority::Scheduled => {
            report.scheduled_voice_start_count =
                report.scheduled_voice_start_count.saturating_add(1);
        }
        StartPriority::Preview => {
            report.preview_voice_start_count = report.preview_voice_start_count.saturating_add(1);
        }
    }
    if trigger_gain.percent() > 0
        && master_volume_percent > 0
        && pad_volume_percent > 0
        && !sample_is_empty
    {
        report.audible_trigger_counts[sample.index()] =
            report.audible_trigger_counts[sample.index()].saturating_add(1);
    }
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

    /// Encode a cheaper, spectrally coarser dither pattern.
    ///
    /// This bounded fallback is used only when the audio service approaches
    /// its DMA deadline. The number of fractional one-bit extensions and the frame-end error are
    /// identical to [`Self::encode`]. A small lookup table merely distributes
    /// those extensions evenly instead of making all sixteen decisions in the
    /// hot path.
    #[inline]
    pub fn encode_coarse(&mut self, sample: i16) -> u32 {
        let unsigned = (i32::from(sample) + 32_768) as u32;
        let quantized = unsigned >> 9;
        let fraction = unsigned & PWM_FRACTION_MASK;
        let total_error = u32::from(self.error) + (fraction << 4);
        let ones = total_error >> 9;
        self.error = (total_error & PWM_FRACTION_MASK) as u16;
        quantized
            | ((PWM_QUANTIZED_MAX - quantized) << 7)
            | (u32::from(COARSE_DITHER_MASKS[ones as usize]) << PWM_COMMAND_BITS)
    }

    pub const fn error(&self) -> u16 {
        self.error
    }
}

pub struct Sequencer<'a> {
    catalog: SampleCatalog<'a>,
    pads: [PadState; BEAT_PAD_COUNT],
    patterns: [Pattern; BEAT_PAD_COUNT],
    trigger_volumes: [TriggerVolumes; BEAT_PAD_COUNT],
    voices: VoicePool,
    pending_preview: Option<PreviewRequest>,
    pending_muted_voice_releases: u16,
    mute_mask: u16,
    global_gain: GainRamp,
    pad_gains: [GainRamp; BEAT_PAD_COUNT],
    base_interval_ms: u32,
    dither: DitherEncoder,
    render_policy: RenderPolicy,
    block_starts_per_pad: [u8; BEAT_PAD_COUNT],
    block_frame_offset: u8,
    reset_release_frames: u8,
}

impl<'a> Sequencer<'a> {
    pub fn new(catalog: SampleCatalog<'a>) -> Self {
        Self {
            catalog,
            pads: core::array::from_fn(PadState::new),
            patterns: [Pattern::all_enabled(); BEAT_PAD_COUNT],
            trigger_volumes: [TriggerVolumes::all_default(); BEAT_PAD_COUNT],
            voices: VoicePool::new(),
            pending_preview: None,
            pending_muted_voice_releases: 0,
            mute_mask: 0,
            global_gain: GainRamp::new(DEFAULT_VOLUME_PERCENT),
            pad_gains: [GainRamp::new(DEFAULT_VOLUME_PERCENT); BEAT_PAD_COUNT],
            base_interval_ms: DEFAULT_BASE_INTERVAL_MS,
            dither: DitherEncoder::new(),
            render_policy: RenderPolicy::FULL,
            block_starts_per_pad: [0; BEAT_PAD_COUNT],
            block_frame_offset: 0,
            reset_release_frames: 0,
        }
    }

    pub fn pads(&self) -> &[PadState; BEAT_PAD_COUNT] {
        &self.pads
    }

    pub const fn base_interval_ms(&self) -> u32 {
        self.base_interval_ms
    }

    pub fn pad_sample(&self, pad: usize) -> Option<SampleId> {
        self.pads.get(pad).map(|state| state.sample)
    }

    pub fn set_pad_sample(&mut self, pad: usize, sample: SampleId) -> bool {
        let Some(state) = self.pads.get_mut(pad) else {
            return false;
        };
        state.sample = sample;
        true
    }

    pub fn set_pad_samples(&mut self, samples: &[SampleId; BEAT_PAD_COUNT]) {
        for (pad, sample) in self.pads.iter_mut().zip(samples.iter().copied()) {
            pad.sample = sample;
        }
    }

    /// Publish a latest-wins preview request for the next rendered block.
    ///
    /// The returned request, when present, was superseded before consumption.
    pub fn queue_preview(&mut self, request: PreviewRequest) -> Option<PreviewRequest> {
        self.pending_preview.replace(request)
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.active_voice_count()
    }

    pub fn active_voice_count_for_pad(&self, pad: usize) -> Option<usize> {
        (pad < BEAT_PAD_COUNT).then(|| self.voices.active_voice_count_for_pad(pad))
    }

    pub fn active_fade_tail_count(&self) -> usize {
        self.voices.active_tail_count()
    }

    /// Select bounded work limits for the next and subsequent render blocks.
    pub fn set_render_policy(&mut self, policy: RenderPolicy) {
        self.render_policy = RenderPolicy {
            max_primary_voices: policy
                .max_primary_voices
                .clamp(1, PRIMARY_VOICE_COUNT as u8),
            max_fade_tails: policy.max_fade_tails.min(FADE_TAIL_COUNT as u8),
            ..policy
        };
    }

    pub const fn render_policy(&self) -> RenderPolicy {
        self.render_policy
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

    pub fn set_trigger_volumes(&mut self, pad: usize, volumes: &TriggerVolumes) -> bool {
        let Some(destination) = self.trigger_volumes.get_mut(pad) else {
            return false;
        };
        *destination = *volumes;
        true
    }

    pub fn trigger_volumes(&self, pad: usize) -> Option<&TriggerVolumes> {
        self.trigger_volumes.get(pad)
    }

    /// Apply the effective mute state at an audio render boundary.
    ///
    /// Newly muted voices begin a short in-place release. Unmuting does not
    /// cancel that release or retrigger a voice.
    pub fn set_mute_mask(&mut self, mute_mask: u16) {
        let mute_mask = mute_mask & BEAT_PAD_MASK;
        let newly_muted = mute_mask & !self.mute_mask;
        self.pending_muted_voice_releases = self
            .pending_muted_voice_releases
            .saturating_add(self.voices.release_mask(newly_muted));
        self.mute_mask = mute_mask;
    }

    pub const fn mute_mask(&self) -> u16 {
        self.mute_mask
    }

    /// Cancel any queued preview and de-click every active primary voice.
    ///
    /// Existing fade tails are already bounded by the same release window and
    /// continue from their current level. This is consumed at an audio-block
    /// boundary by the UI's confirmed Reset-all command.
    pub fn release_all_voices(&mut self) {
        self.pending_preview = None;
        self.pending_muted_voice_releases = self
            .pending_muted_voice_releases
            .saturating_add(self.voices.release_mask(BEAT_PAD_MASK));
        self.reset_release_frames = DECLICK_FRAMES;
    }

    /// Apply master and per-pad gain at an audio render boundary.
    pub fn set_volumes(
        &mut self,
        global_volume_percent: u8,
        pad_volume_percents: &[u8; BEAT_PAD_COUNT],
    ) {
        self.global_gain.set_percent(global_volume_percent);
        for (gain, requested) in self
            .pad_gains
            .iter_mut()
            .zip(pad_volume_percents.iter().copied())
        {
            gain.set_percent(requested);
        }
    }

    pub const fn global_volume_percent(&self) -> u8 {
        self.global_gain.target_percent()
    }

    pub fn pad_volume_percent(&self, pad: usize) -> Option<u8> {
        self.pad_gains.get(pad).map(GainRamp::target_percent)
    }

    /// Apply the base interval and per-pad beat multipliers at a render boundary.
    ///
    /// Changed timing is aligned to the global sample epoch and begins at the
    /// first tick strictly after `from_frame`. Unchanged timing retains phase.
    pub fn apply_timing(
        &mut self,
        beats: &[u16; BEAT_PAD_COUNT],
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
                pad.disable_clock();
            } else {
                pad.align_clock(beats_per_interval, base_interval_ms, from_frame);
            }
        }
    }

    /// Render a block of PIO PWM commands beginning at an absolute frame.
    #[cfg_attr(target_arch = "arm", unsafe(link_section = ".data.ram_func"))]
    #[inline(never)]
    pub fn render(&mut self, start_frame: u64, output: &mut [u32]) -> RenderReport {
        let mut report = RenderReport {
            muted_voice_release_count: core::mem::take(&mut self.pending_muted_voice_releases),
            ..RenderReport::default()
        };
        self.block_starts_per_pad.fill(0);
        // A confirmed musical reset promises a bounded de-click release even
        // if adaptive load control entered Emergency on the same boundary.
        // New clocks are already disabled by the reset snapshot, so preserving
        // these final 32 frames also rapidly reduces subsequent render work.
        if self.reset_release_frames == 0 {
            self.voices.enforce_policy(self.render_policy, &mut report);
        }
        for (offset, word) in output.iter_mut().enumerate() {
            self.block_frame_offset = offset.min(u8::MAX as usize) as u8;
            let frame = start_frame.wrapping_add(offset as u64);
            let mixed = self.render_pcm_frame(frame, &mut report);
            *word = match self.render_policy.dither_quality {
                DitherQuality::Full => self.dither.encode(mixed),
                DitherQuality::Coarse => {
                    report.coarse_dither_frame_count =
                        report.coarse_dither_frame_count.saturating_add(1);
                    self.dither.encode_coarse(mixed)
                }
            };
        }
        report
    }

    fn render_pcm_frame(&mut self, frame: u64, report: &mut RenderReport) -> i16 {
        let mut scheduled = [None; BEAT_PAD_COUNT];

        for (pad_index, pad) in self.pads.iter_mut().enumerate() {
            let trigger_volume = pad.take_due_trigger(
                frame,
                &self.patterns[pad_index],
                &self.trigger_volumes[pad_index],
            );

            if let Some(trigger_volume) = trigger_volume {
                report.latest_visual_triggers[pad_index] = Some(frame);
                if trigger_volume == 0 || self.mute_mask & (1_u16 << pad_index) != 0 {
                    continue;
                }
                scheduled[pad_index] = Some(VoiceStart::with_trigger_gain(
                    pad.sample,
                    TriggerGain::from_percent(trigger_volume),
                ));
            }
        }

        let preview = self.pending_preview.take();
        if scheduled.iter().any(Option::is_some) || preview.is_some() {
            let pad_volume_percents =
                core::array::from_fn(|pad| self.pad_gains[pad].target_percent());
            let master_volume_percent = self.global_gain.target_percent();
            let pad_current_gains_q16 =
                core::array::from_fn(|pad| self.pad_gains[pad].current_q16());
            let allocation = VoiceAllocationState::new(
                master_volume_percent,
                &pad_volume_percents,
                self.global_gain.current_q16(),
                &pad_current_gains_q16,
            );

            for (pad, start) in scheduled.iter().copied().enumerate() {
                let Some(start) = start else {
                    continue;
                };
                let admitted = self.block_starts_per_pad[pad];
                let quota = self.render_policy.max_starts_per_pad;
                let earliest_offset = if quota == 0 {
                    u16::MAX
                } else {
                    u16::from(admitted) * AUDIO_BLOCK_FRAMES as u16 / u16::from(quota)
                };
                if admitted >= quota || u16::from(self.block_frame_offset) < earliest_offset {
                    report.load_shed_trigger_count =
                        report.load_shed_trigger_count.saturating_add(1);
                    continue;
                }
                self.block_starts_per_pad[pad] = self.block_starts_per_pad[pad].saturating_add(1);
                if self.voices.start_with_policy_and_trigger_gain(
                    pad,
                    start,
                    StartPriority::Scheduled,
                    allocation,
                    self.render_policy,
                    report,
                ) {
                    record_voice_start(
                        report,
                        start.sample,
                        start.trigger_gain,
                        pad_volume_percents[pad],
                        master_volume_percent,
                        self.catalog.pcm(start.sample).is_empty(),
                        StartPriority::Scheduled,
                    );
                }
            }

            if let Some(preview) = preview {
                report.latest_visual_triggers[preview.pad] = Some(frame);
                if !self.render_policy.allow_preview {
                    report.load_shed_preview_count =
                        report.load_shed_preview_count.saturating_add(1);
                } else if self.mute_mask & (1_u16 << preview.pad) != 0 {
                    report.preview_drop_count = report.preview_drop_count.saturating_add(1);
                } else if self.voices.start_with_policy(
                    preview.pad,
                    preview.sample,
                    StartPriority::Preview,
                    allocation,
                    self.render_policy,
                    report,
                ) {
                    record_voice_start(
                        report,
                        preview.sample,
                        TriggerGain::FULL,
                        pad_volume_percents[preview.pad],
                        master_volume_percent,
                        self.catalog.pcm(preview.sample).is_empty(),
                        StartPriority::Preview,
                    );
                }
            }
        }

        let active_primaries = self.voices.active_voice_count() as u8;
        let active_tails = self.voices.active_tail_count() as u8;
        report.peak_primary_voice_count = report.peak_primary_voice_count.max(active_primaries);
        report.peak_fade_tail_count = report.peak_fade_tail_count.max(active_tails);
        report.peak_total_voice_count = report
            .peak_total_voice_count
            .max(active_primaries.saturating_add(active_tails));

        // Reset restores gain targets to 100%, but a voice that was silent
        // before reset must not fade upward. Freeze live gains until every
        // reset-released primary and pre-existing fade tail has expired.
        let (pad_gains_q16, master_gain_q16) = if self.reset_release_frames != 0 {
            let pad_gains = core::array::from_fn(|pad| self.pad_gains[pad].current_q16());
            let master_gain = self.global_gain.current_q16();
            self.reset_release_frames -= 1;
            (pad_gains, master_gain)
        } else {
            (
                core::array::from_fn(|pad| self.pad_gains[pad].next_q16()),
                self.global_gain.next_q16(),
            )
        };
        let total = self.voices.render(&self.catalog, &pad_gains_q16);
        let mastered = apply_mix_gain_q16(total, master_gain_q16);
        if mastered > i32::from(i16::MAX) || mastered < i32::from(i16::MIN) {
            report.clipped_frame_count = report.clipped_frame_count.saturating_add(1);
        }
        saturating_i16(mastered)
    }
}

#[cfg(test)]
fn next_ordinal_after(frame: u64, beats_per_interval: u16, base_interval_ms: u32) -> u128 {
    let numerator = u128::from(frame) * 1_000 * u128::from(beats_per_interval);
    let denominator = u128::from(SAMPLE_RATE) * u128::from(base_interval_ms);
    numerator / denominator + 1
}

#[cfg(test)]
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

/// Scale a signed audio accumulator by a linear percentage.
///
/// Division truncates toward zero, preserving symmetry for positive and
/// negative sample values. The wider intermediate supports every `i32` input.
#[inline]
pub fn scale_audio_percent(value: i32, volume_percent: u8) -> i32 {
    ((i64::from(value) * i64::from(volume_percent.min(100))) / 100) as i32
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MuteTarget {
    Global,
    Pad(usize),
}

impl MuteTarget {
    pub const fn for_selected_pad(selected_pad: Option<usize>) -> Self {
        match selected_pad {
            Some(pad) if pad < BEAT_PAD_COUNT => Self::Pad(pad),
            _ => Self::Global,
        }
    }

    const fn is_valid(self) -> bool {
        match self {
            Self::Global => true,
            Self::Pad(pad) => pad < BEAT_PAD_COUNT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VolumeTarget {
    Global,
    Pad(usize),
}

impl VolumeTarget {
    /// Select a pad-local volume when a valid beat key is held, otherwise the
    /// global master volume. Calling this continuously provides live targeting.
    pub const fn for_selected_pad(selected_pad: Option<usize>) -> Self {
        match selected_pad {
            Some(pad) if pad < BEAT_PAD_COUNT => Self::Pad(pad),
            _ => Self::Global,
        }
    }

    const fn is_valid(self) -> bool {
        match self {
            Self::Global => true,
            Self::Pad(pad) => pad < BEAT_PAD_COUNT,
        }
    }
}

/// Volume destination selected by the highlighted row in Pattern mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternVolumeTarget {
    All { pad: usize },
    Step { pad: usize, step: u16 },
}

impl PatternVolumeTarget {
    pub const fn pad(self) -> usize {
        match self {
            Self::All { pad } | Self::Step { pad, .. } => pad,
        }
    }

    const fn is_valid(self) -> bool {
        match self {
            Self::All { pad } => pad < BEAT_PAD_COUNT,
            Self::Step { pad, step } => pad < BEAT_PAD_COUNT && (step as usize) < PATTERN_BITS,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MuteRelease {
    pub target: MuteTarget,
    pub tapped: bool,
}

/// Tracks one debounced press of the physical mute key.
///
/// The target is captured at the press edge so later selection changes do not
/// retarget an in-progress gesture.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MuteButtonState {
    active: Option<(MuteTarget, u64)>,
}

impl MuteButtonState {
    pub const fn new() -> Self {
        Self { active: None }
    }

    /// Capture a gesture. Returns false for an invalid target or duplicate
    /// press while another gesture is active.
    pub fn press(&mut self, target: MuteTarget, now_ms: u64) -> bool {
        if self.active.is_some() || !target.is_valid() {
            return false;
        }
        self.active = Some((target, now_ms));
        true
    }

    pub const fn active_target(&self) -> Option<MuteTarget> {
        match self.active {
            Some((target, _)) => Some(target),
            None => None,
        }
    }

    /// Finish the active gesture. Exactly 300 ms is a hold, not a tap.
    pub fn release(&mut self, now_ms: u64) -> Option<MuteRelease> {
        let (target, pressed_at_ms) = self.active.take()?;
        Some(MuteRelease {
            target,
            tapped: now_ms.wrapping_sub(pressed_at_ms) < MUTE_TAP_THRESHOLD_MS,
        })
    }

    /// Discard an in-progress gesture without producing a tap toggle.
    pub fn cancel(&mut self) -> Option<MuteTarget> {
        self.active.take().map(|(target, _)| target)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MuteScanAction {
    Cancel(MuteTarget),
    Release(MuteRelease),
}

/// Resolve same-scan Mute/Return edges with Return taking precedence.
pub fn resolve_mute_scan(
    button: &mut MuteButtonState,
    return_pressed: bool,
    mute_released: bool,
    now_ms: u64,
) -> Option<MuteScanAction> {
    if return_pressed {
        button.cancel().map(MuteScanAction::Cancel)
    } else if mute_released {
        button.release(now_ms).map(MuteScanAction::Release)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SharedState {
    pub desired_beats: [u16; BEAT_PAD_COUNT],
    pad_samples: [SampleId; BEAT_PAD_COUNT],
    pending_preview: Option<PreviewRequest>,
    pub base_interval_ms: u32,
    pub led_brightness_percent: u8,
    pub playback_frame: u64,
    pub latest_trigger_frames: [u64; BEAT_PAD_COUNT],
    pub underrun_count: u32,
    pub last_render_time_us: u32,
    pub max_render_time_us: u32,
    pub render_deadline_miss_count: u32,
    pub last_audio_service_time_us: u32,
    pub max_audio_service_time_us: u32,
    pub audio_service_deadline_miss_count: u32,
    pub max_dma_cadence_us: u32,
    pub max_dma_handoff_us: u32,
    pub audio_load_level: LoadLevel,
    pub effective_voice_limit: u8,
    pub min_effective_voice_limit: u8,
    pub audio_load_ewma_us: u32,
    pub audio_load_window_max_us: u32,
    pub audio_load_transition_count: u32,
    pub last_peak_primary_voices: u8,
    pub max_peak_primary_voices: u8,
    pub worst_service_primary_voices: u8,
    pub worst_service_voice_limit: u8,
    pub worst_service_load_level: LoadLevel,
    pub sampler_diagnostics: SamplerDiagnostics,
    patterns: [Pattern; BEAT_PAD_COUNT],
    trigger_volumes: [TriggerVolumes; BEAT_PAD_COUNT],
    pattern_dirty_mask: u16,
    pub pattern_revision: u32,
    global_mute_latched: bool,
    pad_mute_latched: [bool; BEAT_PAD_COUNT],
    momentary_mute_target: Option<MuteTarget>,
    global_volume_percent: u8,
    pad_volume_percents: [u8; BEAT_PAD_COUNT],
    release_all_requested: bool,
    /// Monotonic edit generation used to compare the live song with a saved
    /// snapshot. It is runtime metadata and is never loaded from flash.
    pub song_revision: u32,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            desired_beats: [0; BEAT_PAD_COUNT],
            pad_samples: DEFAULT_PAD_SAMPLES,
            pending_preview: None,
            base_interval_ms: DEFAULT_BASE_INTERVAL_MS,
            led_brightness_percent: DEFAULT_LED_BRIGHTNESS_PERCENT,
            playback_frame: 0,
            latest_trigger_frames: [0; BEAT_PAD_COUNT],
            underrun_count: 0,
            last_render_time_us: 0,
            max_render_time_us: 0,
            render_deadline_miss_count: 0,
            last_audio_service_time_us: 0,
            max_audio_service_time_us: 0,
            audio_service_deadline_miss_count: 0,
            max_dma_cadence_us: 0,
            max_dma_handoff_us: 0,
            audio_load_level: LoadLevel::Normal,
            effective_voice_limit: PRIMARY_VOICE_COUNT as u8,
            min_effective_voice_limit: PRIMARY_VOICE_COUNT as u8,
            audio_load_ewma_us: 0,
            audio_load_window_max_us: 0,
            audio_load_transition_count: 0,
            last_peak_primary_voices: 0,
            max_peak_primary_voices: 0,
            worst_service_primary_voices: 0,
            worst_service_voice_limit: PRIMARY_VOICE_COUNT as u8,
            worst_service_load_level: LoadLevel::Normal,
            sampler_diagnostics: SamplerDiagnostics::default(),
            patterns: [Pattern::all_enabled(); BEAT_PAD_COUNT],
            trigger_volumes: [TriggerVolumes::all_default(); BEAT_PAD_COUNT],
            pattern_dirty_mask: 0,
            pattern_revision: 0,
            global_mute_latched: false,
            pad_mute_latched: [false; BEAT_PAD_COUNT],
            momentary_mute_target: None,
            global_volume_percent: DEFAULT_VOLUME_PERCENT,
            pad_volume_percents: [DEFAULT_VOLUME_PERCENT; BEAT_PAD_COUNT],
            release_all_requested: false,
            song_revision: 0,
        }
    }
}

impl SharedState {
    /// Set the global interval without bypassing persistent dirty tracking.
    pub fn set_base_interval_ms(&mut self, interval_ms: u32) -> bool {
        if interval_ms < MIN_BASE_INTERVAL_MS {
            return false;
        }
        if self.base_interval_ms != interval_ms {
            self.base_interval_ms = interval_ms;
            self.mark_song_changed();
        }
        true
    }

    /// Set one pad division without bypassing persistent dirty tracking.
    pub fn set_desired_beats(&mut self, pad: usize, beats: u16) -> bool {
        if pad >= BEAT_PAD_COUNT || beats > MAX_BEAT_MULTIPLIER {
            return false;
        }
        if self.desired_beats[pad] != beats {
            self.desired_beats[pad] = beats;
            self.mark_song_changed();
        }
        true
    }

    pub const fn pad_samples(&self) -> &[SampleId; BEAT_PAD_COUNT] {
        &self.pad_samples
    }

    pub fn pad_sample(&self, pad: usize) -> Option<SampleId> {
        self.pad_samples.get(pad).copied()
    }

    pub fn set_pad_sample(&mut self, pad: usize, sample: SampleId) -> bool {
        let Some(destination) = self.pad_samples.get_mut(pad) else {
            return false;
        };
        let changed = *destination != sample;
        *destination = sample;
        if changed {
            self.mark_song_changed();
        }
        true
    }

    pub fn queue_preview(&mut self, request: PreviewRequest) -> Option<PreviewRequest> {
        self.pending_preview.replace(request)
    }

    pub fn take_preview(&mut self) -> Option<PreviewRequest> {
        self.pending_preview.take()
    }

    /// Restore every musical control to its boot value without disturbing
    /// brightness, the playback epoch, adaptive-load state, or diagnostics.
    pub fn reset_musical_state(&mut self) {
        self.desired_beats = [0; BEAT_PAD_COUNT];
        self.pad_samples = DEFAULT_PAD_SAMPLES;
        self.pending_preview = None;
        self.base_interval_ms = DEFAULT_BASE_INTERVAL_MS;
        self.latest_trigger_frames = [0; BEAT_PAD_COUNT];
        self.patterns = [Pattern::all_enabled(); BEAT_PAD_COUNT];
        self.trigger_volumes = [TriggerVolumes::all_default(); BEAT_PAD_COUNT];
        self.pattern_dirty_mask |= BEAT_PAD_MASK;
        self.pattern_revision = self.pattern_revision.wrapping_add(1);
        self.global_mute_latched = false;
        self.pad_mute_latched = [false; BEAT_PAD_COUNT];
        self.momentary_mute_target = None;
        self.global_volume_percent = DEFAULT_VOLUME_PERCENT;
        self.pad_volume_percents = [DEFAULT_VOLUME_PERCENT; BEAT_PAD_COUNT];
        self.release_all_requested = true;
        self.mark_song_changed();
    }

    /// Record one logical persistent-state edit. Callers that mutate the
    /// public timing fields directly must call this once for that edit.
    pub fn mark_song_changed(&mut self) {
        self.song_revision = self.song_revision.wrapping_add(1);
    }

    /// Consume the latest Reset-all request at an audio-block boundary.
    pub fn take_release_all_request(&mut self) -> bool {
        core::mem::take(&mut self.release_all_requested)
    }

    pub fn record_sampler_report(&mut self, report: &RenderReport) {
        self.sampler_diagnostics.record(report);
    }

    pub fn pattern(&self, pad: usize) -> Option<&Pattern> {
        self.patterns.get(pad)
    }

    pub fn trigger_volumes(&self, pad: usize) -> Option<&TriggerVolumes> {
        self.trigger_volumes.get(pad)
    }

    pub fn trigger_volume(&self, pad: usize, step: usize) -> Option<u8> {
        self.trigger_volumes.get(pad)?.percent(step)
    }

    /// Resolve the value represented by a Pattern-mode Volume target. The
    /// `All` row reports the rounded average of its 256 stored levels.
    pub fn pattern_volume_percent(&self, target: PatternVolumeTarget) -> Option<u8> {
        if !target.is_valid() {
            return None;
        }
        match target {
            PatternVolumeTarget::All { pad } => Some(self.trigger_volumes[pad].average_percent()),
            PatternVolumeTarget::Step { pad, step } => {
                self.trigger_volumes[pad].percent(usize::from(step))
            }
        }
    }

    /// Adjust one trigger level or every level in a pad by percentage points.
    /// A whole-map edit includes hidden and disabled slots and advances the
    /// shared pattern revision only once.
    pub fn adjust_pattern_volume(&mut self, target: PatternVolumeTarget, delta: i32) -> Option<u8> {
        if !target.is_valid() {
            return None;
        }
        let (pad, changed, result) = match target {
            PatternVolumeTarget::All { pad } => {
                let changed = self.trigger_volumes[pad].adjust_all(delta);
                let result = self.trigger_volumes[pad].average_percent();
                (pad, changed, result)
            }
            PatternVolumeTarget::Step { pad, step } => {
                pattern_step_index(step, self.desired_beats[pad])?;
                let previous = self.trigger_volumes[pad].percent(usize::from(step))?;
                let result = self.trigger_volumes[pad].adjust_step(usize::from(step), delta)?;
                (pad, result != previous, result)
            }
        };
        if changed {
            self.pattern_dirty_mask |= 1 << pad;
            self.pattern_revision = self.pattern_revision.wrapping_add(1);
            self.mark_song_changed();
        }
        Some(result)
    }

    pub fn toggle_pattern_step(&mut self, pad: usize, step: u16) -> Option<bool> {
        let division = *self.desired_beats.get(pad)?;
        let enabled = self.patterns.get_mut(pad)?.toggle_step(step, division)?;
        self.pattern_dirty_mask |= 1 << pad;
        self.pattern_revision = self.pattern_revision.wrapping_add(1);
        self.mark_song_changed();
        Some(enabled)
    }

    pub fn set_pattern_all(&mut self, pad: usize, enabled: bool) -> bool {
        if pad >= BEAT_PAD_COUNT {
            return false;
        }
        let previous_pattern = self.patterns[pad];
        let previous_volumes = self.trigger_volumes[pad];
        self.patterns[pad].fill(enabled);
        // Both explicit whole-map choices establish a predictable baseline:
        // All enables every trigger and None disables every trigger, while
        // either choice clears all per-trigger accents back to 100%.
        self.trigger_volumes[pad] = TriggerVolumes::all_default();
        self.pattern_dirty_mask |= 1 << pad;
        self.pattern_revision = self.pattern_revision.wrapping_add(1);
        if self.patterns[pad] != previous_pattern || self.trigger_volumes[pad] != previous_volumes {
            self.mark_song_changed();
        }
        true
    }

    pub fn take_pattern_dirty_mask(&mut self) -> u16 {
        let dirty = self.pattern_dirty_mask;
        self.pattern_dirty_mask = 0;
        dirty
    }

    pub fn latched_mute(&self, target: MuteTarget) -> Option<bool> {
        match target {
            MuteTarget::Global => Some(self.global_mute_latched),
            MuteTarget::Pad(pad) => self.pad_mute_latched.get(pad).copied(),
        }
    }

    /// Start the momentary overlay for a captured gesture.
    pub fn begin_mute_gesture(&mut self, target: MuteTarget) -> bool {
        if !target.is_valid() || self.momentary_mute_target.is_some() {
            return false;
        }
        self.momentary_mute_target = Some(target);
        true
    }

    /// Atomically clear the momentary overlay and, for a tap, toggle its
    /// captured persistent mute.
    pub fn end_mute_gesture(&mut self, release: MuteRelease) -> bool {
        if self.momentary_mute_target != Some(release.target) {
            return false;
        }
        if release.tapped {
            match release.target {
                MuteTarget::Global => self.global_mute_latched = !self.global_mute_latched,
                MuteTarget::Pad(pad) => {
                    self.pad_mute_latched[pad] = !self.pad_mute_latched[pad];
                }
            }
            self.mark_song_changed();
        }
        self.momentary_mute_target = None;
        true
    }

    /// Clear a momentary mute without changing its persistent latch.
    pub fn cancel_mute_gesture(&mut self) -> Option<MuteTarget> {
        self.momentary_mute_target.take()
    }

    /// The local status shown by the mute key: global state for the global
    /// target, or only the selected pad's state for a pad target.
    pub fn mute_indicator_active(&self, target: MuteTarget) -> Option<bool> {
        Some(self.latched_mute(target)? || self.momentary_mute_target == Some(target))
    }

    /// Produce the pad mask consumed by the real-time sequencer.
    pub fn effective_mute_mask(&self) -> u16 {
        if self.global_mute_latched || self.momentary_mute_target == Some(MuteTarget::Global) {
            return BEAT_PAD_MASK;
        }

        let mut mask = 0_u16;
        for (pad, muted) in self.pad_mute_latched.iter().copied().enumerate() {
            if muted || self.momentary_mute_target == Some(MuteTarget::Pad(pad)) {
                mask |= 1_u16 << pad;
            }
        }
        mask
    }

    pub const fn global_volume_percent(&self) -> u8 {
        self.global_volume_percent
    }

    pub const fn pad_volume_percents(&self) -> &[u8; BEAT_PAD_COUNT] {
        &self.pad_volume_percents
    }

    pub fn volume_percent(&self, target: VolumeTarget) -> Option<u8> {
        match target {
            VolumeTarget::Global => Some(self.global_volume_percent),
            VolumeTarget::Pad(pad) => self.pad_volume_percents.get(pad).copied(),
        }
    }

    /// Adjust a master or per-pad percentage, clamped to the supported range.
    pub fn adjust_volume(&mut self, target: VolumeTarget, delta: i32) -> Option<u8> {
        if !target.is_valid() {
            return None;
        }
        let volume = match target {
            VolumeTarget::Global => &mut self.global_volume_percent,
            VolumeTarget::Pad(pad) => &mut self.pad_volume_percents[pad],
        };
        let previous = *volume;
        *volume = adjust_volume_percent(*volume, delta);
        let result = *volume;
        if result != previous {
            self.mark_song_changed();
        }
        Some(result)
    }
}

/// Stable identifier for one of the 256 user-visible song slots.
///
/// Slots are stored as zero-based keys, while the UI presents them as
/// `001` through `256`. The associated animal names are firmware metadata and
/// are deliberately not duplicated in every flash record.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct SongSlot(u8);

impl SongSlot {
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < SONG_SLOT_COUNT {
            Some(Self(index as u8))
        } else {
            None
        }
    }

    pub const fn from_number(number: u16) -> Option<Self> {
        if number >= 1 && number <= SONG_SLOT_COUNT as u16 {
            Some(Self((number - 1) as u8))
        } else {
            None
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }

    pub const fn number(self) -> u16 {
        self.0 as u16 + 1
    }

    pub const fn storage_key(self) -> u8 {
        self.0
    }

    pub fn animal_name(self) -> &'static str {
        SONG_SLOT_ANIMAL_NAMES[self.index()]
    }
}

/// Frozen display names for [`SongSlot`]. Never reorder this list after a
/// release: a slot number and its animal name form one stable user identity.
pub const SONG_SLOT_ANIMAL_NAMES: [&str; SONG_SLOT_COUNT] = [
    "Aardvark",
    "Albatross",
    "Alligator",
    "Alpaca",
    "Anaconda",
    "Anchovy",
    "Angelfish",
    "Ant",
    "Anteater",
    "Antelope",
    "Ape",
    "Armadillo",
    "Axolotl",
    "Baboon",
    "Badger",
    "Barracuda",
    "Basilisk",
    "Bat",
    "Bear",
    "Beaver",
    "Bee",
    "Beetle",
    "Bison",
    "Bluebird",
    "Boar",
    "Bobcat",
    "Buffalo",
    "Butterfly",
    "Buzzard",
    "Camel",
    "Capybara",
    "Cardinal",
    "Caribou",
    "Cassowary",
    "Cat",
    "Caterpillar",
    "Catfish",
    "Centipede",
    "Chameleon",
    "Cheetah",
    "Chickadee",
    "Chicken",
    "Chimpanzee",
    "Chinchilla",
    "Chipmunk",
    "Clam",
    "Cobra",
    "Cockatoo",
    "Cod",
    "Condor",
    "Coral",
    "Cougar",
    "Cow",
    "Coyote",
    "Crab",
    "Crane",
    "Crayfish",
    "Cricket",
    "Crocodile",
    "Crow",
    "Cuckoo",
    "Curlew",
    "Deer",
    "Dingo",
    "Dolphin",
    "Donkey",
    "Dove",
    "Dragonfly",
    "Duck",
    "Dugong",
    "Eagle",
    "Earthworm",
    "Echidna",
    "Eel",
    "Egret",
    "Elephant",
    "Elk",
    "Emu",
    "Falcon",
    "Ferret",
    "Finch",
    "Firefly",
    "Flamingo",
    "Flea",
    "Fly",
    "Fox",
    "Frog",
    "Gazelle",
    "Gecko",
    "Gerbil",
    "Gibbon",
    "Giraffe",
    "Gnat",
    "Goat",
    "Goldfish",
    "Goose",
    "Gopher",
    "Gorilla",
    "Grasshopper",
    "Grouse",
    "Guppy",
    "Hamster",
    "Hare",
    "Hawk",
    "Hedgehog",
    "Heron",
    "Herring",
    "Hippo",
    "Hornet",
    "Horse",
    "Hummingbird",
    "Hyena",
    "Ibex",
    "Ibis",
    "Iguana",
    "Impala",
    "Jackal",
    "Jaguar",
    "Jay",
    "Jellyfish",
    "Jerboa",
    "Kangaroo",
    "Kingfisher",
    "Kiwi",
    "Koala",
    "Koi",
    "Komodo",
    "Krill",
    "Ladybug",
    "Lark",
    "Lemur",
    "Leopard",
    "Liger",
    "Lion",
    "Lizard",
    "Llama",
    "Lobster",
    "Locust",
    "Lynx",
    "Macaque",
    "Macaw",
    "Magpie",
    "Mallard",
    "Manatee",
    "Mantis",
    "Marmot",
    "Meerkat",
    "Mink",
    "Minnow",
    "Mole",
    "Mongoose",
    "Monkey",
    "Moose",
    "Mosquito",
    "Moth",
    "Mouse",
    "Mule",
    "Narwhal",
    "Nautilus",
    "Newt",
    "Nightingale",
    "Numbat",
    "Ocelot",
    "Octopus",
    "Okapi",
    "Opossum",
    "Orangutan",
    "Orca",
    "Osprey",
    "Ostrich",
    "Otter",
    "Owl",
    "Ox",
    "Oyster",
    "Panda",
    "Panther",
    "Parrot",
    "Peacock",
    "Pelican",
    "Penguin",
    "Pheasant",
    "Pig",
    "Pigeon",
    "Pike",
    "Platypus",
    "Pony",
    "Porcupine",
    "Porpoise",
    "Possum",
    "Prawn",
    "Puffin",
    "Puma",
    "Python",
    "Quail",
    "Quokka",
    "Rabbit",
    "Raccoon",
    "Ram",
    "Rat",
    "Raven",
    "Ray",
    "Reindeer",
    "Rhino",
    "Roadrunner",
    "Robin",
    "Rooster",
    "Salamander",
    "Salmon",
    "Sandpiper",
    "Sardine",
    "Scorpion",
    "Seahorse",
    "Seal",
    "Shark",
    "Sheep",
    "Shrew",
    "Shrimp",
    "Skunk",
    "Sloth",
    "Snail",
    "Snake",
    "Sparrow",
    "Spider",
    "Squid",
    "Squirrel",
    "Starfish",
    "Stingray",
    "Stork",
    "Swallow",
    "Swan",
    "Tapir",
    "Tarantula",
    "Termite",
    "Tern",
    "Tiger",
    "Toad",
    "Toucan",
    "Trout",
    "Tuna",
    "Turkey",
    "Turtle",
    "Viper",
    "Vole",
    "Vulture",
    "Wallaby",
    "Walrus",
    "Wasp",
    "Weasel",
    "Whale",
    "Wolf",
    "Wombat",
    "Woodpecker",
    "Worm",
    "Yak",
    "Zebra",
    "Zebu",
];

/// Versioned song-record envelope written inside the flash map value.
pub const SONG_FORMAT_MAGIC: [u8; 4] = *b"LTSG";
pub const SONG_FORMAT_VERSION: u16 = 1;
pub const SONG_FORMAT_HEADER_LEN: usize = 8;
/// Number of pads encoded by the frozen V1 song schema.
pub const SONG_V1_PAD_COUNT: usize = 9;
/// Bytes in one V1 pattern-enable map.
pub const SONG_V1_PATTERN_BYTES: usize = 32;
/// Number of 32-byte trigger-level chunks encoded for one V1 pad.
pub const SONG_V1_TRIGGER_LEVEL_CHUNKS: usize = 8;
/// Exact encoded length of [`StoredSongV1::default`] with the pinned codec.
///
/// This is a schema regression sentinel, not the maximum possible record size.
pub const SONG_V1_DEFAULT_ENCODED_LEN: usize = 2_640;
/// Includes the eight-byte envelope. The current maximum encoded size is
/// tested to fit. The spare buffer space is not permission to change the
/// frozen V1 schema; incompatible fields require a new format version.
pub const SONG_ENCODED_MAX_LEN: usize = 3_072;

/// Backward-compatible name for the frozen V1 trigger-level chunk count.
pub const STORED_TRIGGER_LEVEL_CHUNKS: usize = SONG_V1_TRIGGER_LEVEL_CHUNKS;

// V1's serialized dimensions must never follow later runtime geometry changes.
// A runtime change must either preserve these conversion invariants or introduce
// a new stored-song version with an explicit migration path.
const _: () = {
    assert!(BEAT_PAD_COUNT == SONG_V1_PAD_COUNT);
    assert!(PATTERN_BYTES == SONG_V1_PATTERN_BYTES);
    assert!(PATTERN_BITS == SONG_V1_PATTERN_BYTES * 8);
    assert!(PATTERN_BITS == SONG_V1_PATTERN_BYTES * SONG_V1_TRIGGER_LEVEL_CHUNKS);
};

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StoredPadV1 {
    pub division: u16,
    pub sample_id: u8,
    pub pattern: [u8; SONG_V1_PATTERN_BYTES],
    pub trigger_levels: [[u8; SONG_V1_PATTERN_BYTES]; SONG_V1_TRIGGER_LEVEL_CHUNKS],
    pub mute: bool,
    pub volume_percent: u8,
}

/// Frozen schema for the first persistent LoopTic song format.
///
/// Runtime state such as playback position, active voices, previews, UI
/// cursors, brightness, and diagnostics is intentionally absent.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct StoredSongV1 {
    pub base_interval_ms: u32,
    pub global_mute: bool,
    pub master_volume_percent: u8,
    pub pads: [StoredPadV1; SONG_V1_PAD_COUNT],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SongValidationError {
    BaseIntervalTooShort { value: u32 },
    DivisionOutOfRange { pad: u8, value: u16 },
    SampleOutOfRange { pad: u8, value: u8 },
    MasterVolumeOutOfRange { value: u8 },
    PadVolumeOutOfRange { pad: u8, value: u8 },
    TriggerVolumeOutOfRange { pad: u8, step: u16, value: u8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SongEncodeError {
    InvalidSong(SongValidationError),
    BufferTooSmall,
    PayloadTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SongDecodeError {
    Truncated,
    BadMagic { found: [u8; 4] },
    UnsupportedVersion { found: u16, supported: u16 },
    LengthMismatch { declared: u16, actual: usize },
    InvalidPayload,
    InvalidSong(SongValidationError),
}

impl StoredSongV1 {
    pub fn snapshot(state: &SharedState) -> Self {
        let pads = core::array::from_fn(|pad| {
            let mut trigger_levels = [[0_u8; SONG_V1_PATTERN_BYTES]; SONG_V1_TRIGGER_LEVEL_CHUNKS];
            for (chunk_index, chunk) in trigger_levels.iter_mut().enumerate() {
                let start = chunk_index * SONG_V1_PATTERN_BYTES;
                chunk.copy_from_slice(
                    &state.trigger_volumes[pad].percents[start..start + SONG_V1_PATTERN_BYTES],
                );
            }
            StoredPadV1 {
                division: state.desired_beats[pad],
                sample_id: state.pad_samples[pad].index() as u8,
                pattern: state.patterns[pad].bits,
                trigger_levels,
                mute: state.pad_mute_latched[pad],
                volume_percent: state.pad_volume_percents[pad],
            }
        });
        Self {
            base_interval_ms: state.base_interval_ms,
            global_mute: state.global_mute_latched,
            master_volume_percent: state.global_volume_percent,
            pads,
        }
    }

    pub fn validate(&self) -> Result<(), SongValidationError> {
        if self.base_interval_ms < MIN_BASE_INTERVAL_MS {
            return Err(SongValidationError::BaseIntervalTooShort {
                value: self.base_interval_ms,
            });
        }
        if self.master_volume_percent > 100 {
            return Err(SongValidationError::MasterVolumeOutOfRange {
                value: self.master_volume_percent,
            });
        }
        for (pad, stored) in self.pads.iter().enumerate() {
            let pad = pad as u8;
            if stored.division > MAX_BEAT_MULTIPLIER {
                return Err(SongValidationError::DivisionOutOfRange {
                    pad,
                    value: stored.division,
                });
            }
            if usize::from(stored.sample_id) >= SAMPLE_COUNT {
                return Err(SongValidationError::SampleOutOfRange {
                    pad,
                    value: stored.sample_id,
                });
            }
            if stored.volume_percent > 100 {
                return Err(SongValidationError::PadVolumeOutOfRange {
                    pad,
                    value: stored.volume_percent,
                });
            }
            for (chunk_index, chunk) in stored.trigger_levels.iter().enumerate() {
                for (offset, &value) in chunk.iter().enumerate() {
                    if value > 100 {
                        return Err(SongValidationError::TriggerVolumeOutOfRange {
                            pad,
                            step: (chunk_index * SONG_V1_PATTERN_BYTES + offset) as u16,
                            value,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Atomically replace persistent musical state after complete validation.
    /// Runtime clock, brightness, adaptive-load state, and diagnostics survive.
    pub fn apply_to(&self, state: &mut SharedState) -> Result<(), SongValidationError> {
        self.validate()?;

        let mut samples = DEFAULT_PAD_SAMPLES;
        let mut patterns = [Pattern::all_enabled(); BEAT_PAD_COUNT];
        let mut trigger_volumes = [TriggerVolumes::all_default(); BEAT_PAD_COUNT];
        let mut pad_mutes = [false; BEAT_PAD_COUNT];
        let mut pad_volumes = [DEFAULT_VOLUME_PERCENT; BEAT_PAD_COUNT];
        for (pad, stored) in self.pads.iter().enumerate() {
            samples[pad] = SampleId::from_index(usize::from(stored.sample_id))
                .expect("validated sample identifier");
            patterns[pad] = Pattern {
                bits: stored.pattern,
            };
            let mut percents = [0_u8; PATTERN_BITS];
            for (chunk_index, chunk) in stored.trigger_levels.iter().enumerate() {
                let start = chunk_index * SONG_V1_PATTERN_BYTES;
                percents[start..start + SONG_V1_PATTERN_BYTES].copy_from_slice(chunk);
            }
            let sum = percents.iter().map(|&value| u32::from(value)).sum();
            trigger_volumes[pad] = TriggerVolumes { percents, sum };
            pad_mutes[pad] = stored.mute;
            pad_volumes[pad] = stored.volume_percent;
        }

        state.desired_beats = core::array::from_fn(|pad| self.pads[pad].division);
        state.pad_samples = samples;
        state.pending_preview = None;
        state.base_interval_ms = self.base_interval_ms;
        state.latest_trigger_frames = [0; BEAT_PAD_COUNT];
        state.patterns = patterns;
        state.trigger_volumes = trigger_volumes;
        state.pattern_dirty_mask |= BEAT_PAD_MASK;
        state.pattern_revision = state.pattern_revision.wrapping_add(1);
        state.global_mute_latched = self.global_mute;
        state.pad_mute_latched = pad_mutes;
        state.momentary_mute_target = None;
        state.global_volume_percent = self.master_volume_percent;
        state.pad_volume_percents = pad_volumes;
        state.release_all_requested = true;
        state.mark_song_changed();
        Ok(())
    }
}

impl Default for StoredSongV1 {
    fn default() -> Self {
        Self::snapshot(&SharedState::default())
    }
}

/// Encode one validated V1 song into a self-describing, allocation-free value.
pub fn encode_song_v1<'a>(
    song: &StoredSongV1,
    output: &'a mut [u8],
) -> Result<&'a [u8], SongEncodeError> {
    song.validate().map_err(SongEncodeError::InvalidSong)?;
    if output.len() < SONG_FORMAT_HEADER_LEN {
        return Err(SongEncodeError::BufferTooSmall);
    }
    let payload_len = postcard::to_slice(song, &mut output[SONG_FORMAT_HEADER_LEN..])
        .map_err(|_| SongEncodeError::BufferTooSmall)?
        .len();
    let payload_len = u16::try_from(payload_len).map_err(|_| SongEncodeError::PayloadTooLarge)?;
    output[..4].copy_from_slice(&SONG_FORMAT_MAGIC);
    output[4..6].copy_from_slice(&SONG_FORMAT_VERSION.to_le_bytes());
    output[6..8].copy_from_slice(&payload_len.to_le_bytes());
    Ok(&output[..SONG_FORMAT_HEADER_LEN + usize::from(payload_len)])
}

/// Decode a song envelope and report unsupported schema versions separately
/// from corruption, truncation, and semantically invalid values.
pub fn decode_song(bytes: &[u8]) -> Result<StoredSongV1, SongDecodeError> {
    if bytes.len() < SONG_FORMAT_HEADER_LEN {
        return Err(SongDecodeError::Truncated);
    }
    let found_magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if found_magic != SONG_FORMAT_MAGIC {
        return Err(SongDecodeError::BadMagic { found: found_magic });
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != SONG_FORMAT_VERSION {
        return Err(SongDecodeError::UnsupportedVersion {
            found: version,
            supported: SONG_FORMAT_VERSION,
        });
    }
    let declared = u16::from_le_bytes([bytes[6], bytes[7]]);
    let actual = bytes.len() - SONG_FORMAT_HEADER_LEN;
    if actual < usize::from(declared) {
        return Err(SongDecodeError::Truncated);
    }
    if actual != usize::from(declared) {
        return Err(SongDecodeError::LengthMismatch { declared, actual });
    }
    let (song, remainder): (StoredSongV1, &[u8]) =
        postcard::take_from_bytes(&bytes[SONG_FORMAT_HEADER_LEN..])
            .map_err(|_| SongDecodeError::InvalidPayload)?;
    if !remainder.is_empty() {
        return Err(SongDecodeError::InvalidPayload);
    }
    song.validate().map_err(SongDecodeError::InvalidSong)?;
    Ok(song)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeyChanges {
    pub pressed: u16,
    pub released: u16,
}

/// Persistent beat-pad selection. The storage supports any combination of
/// pads even though the current controller applies an exclusive policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VoiceSelection {
    mask: u16,
}

impl VoiceSelection {
    pub const fn new() -> Self {
        Self { mask: 0 }
    }

    pub const fn from_mask(mask: u16) -> Self {
        Self {
            mask: mask & BEAT_PAD_MASK,
        }
    }

    pub const fn mask(self) -> u16 {
        self.mask
    }

    pub const fn contains(self, pad: usize) -> bool {
        pad < BEAT_PAD_COUNT && self.mask & (1_u16 << pad) != 0
    }

    pub const fn count(self) -> u32 {
        self.mask.count_ones()
    }

    /// Return the selected pad only when the selection is exclusive.
    pub const fn selected(self) -> Option<usize> {
        if self.mask.count_ones() == 1 {
            Some(self.mask.trailing_zeros() as usize)
        } else {
            None
        }
    }

    pub fn insert(&mut self, pad: usize) -> bool {
        if pad >= BEAT_PAD_COUNT {
            return false;
        }
        self.mask |= 1_u16 << pad;
        true
    }

    pub fn remove(&mut self, pad: usize) -> bool {
        if pad >= BEAT_PAD_COUNT {
            return false;
        }
        self.mask &= !(1_u16 << pad);
        true
    }

    pub fn toggle(&mut self, pad: usize) -> bool {
        if pad >= BEAT_PAD_COUNT {
            return false;
        }
        self.mask ^= 1_u16 << pad;
        true
    }

    /// Toggle one pad under the current exclusive-selection policy.
    pub fn toggle_exclusive(&mut self, pad: usize) -> bool {
        if pad >= BEAT_PAD_COUNT {
            return false;
        }
        let bit = 1_u16 << pad;
        self.mask = if self.mask == bit { 0 } else { bit };
        true
    }

    pub fn select_exclusive(&mut self, pad: usize) -> bool {
        if pad >= BEAT_PAD_COUNT {
            return false;
        }
        self.mask = 1_u16 << pad;
        true
    }

    pub fn clear(&mut self) {
        self.mask = 0;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum RootMode {
    #[default]
    Beats,
    Pattern,
    Sample,
    Light,
    Save,
    Songs,
    ResetAll,
}

impl RootMode {
    pub const ALL: [Self; 7] = [
        Self::Beats,
        Self::Pattern,
        Self::Sample,
        Self::Light,
        Self::Save,
        Self::Songs,
        Self::ResetAll,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn from_index(index: usize) -> Self {
        Self::ALL[if index < Self::COUNT {
            index
        } else {
            Self::COUNT - 1
        }]
    }

    pub fn clamped_offset(self, delta: i32) -> Self {
        let index = (self.index() as i32)
            .saturating_add(delta)
            .clamp(0, Self::COUNT as i32 - 1) as usize;
        Self::from_index(index)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UiPage {
    #[default]
    Root,
    Pattern,
    Beats,
    Sample,
    Light,
    Songs,
    ResetAll,
}

/// Occupied-song index built while scanning the flash map at boot.
///
/// Keeping this as eight machine words makes slot lookup constant time and
/// lets the OLED task take a cheap snapshot without caching song payloads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SongSlotOccupancy {
    words: [u32; SONG_SLOT_COUNT / 32],
}

impl SongSlotOccupancy {
    pub const fn empty() -> Self {
        Self {
            words: [0; SONG_SLOT_COUNT / 32],
        }
    }

    pub const fn from_words(words: [u32; SONG_SLOT_COUNT / 32]) -> Self {
        Self { words }
    }

    pub const fn words(&self) -> &[u32; SONG_SLOT_COUNT / 32] {
        &self.words
    }

    pub const fn contains(self, slot: SongSlot) -> bool {
        let index = slot.index();
        self.words[index / 32] & (1_u32 << (index % 32)) != 0
    }

    pub fn set(&mut self, slot: SongSlot, occupied: bool) {
        let index = slot.index();
        let bit = 1_u32 << (index % 32);
        if occupied {
            self.words[index / 32] |= bit;
        } else {
            self.words[index / 32] &= !bit;
        }
    }

    pub fn count(self) -> u32 {
        self.words.iter().map(|word| word.count_ones()).sum()
    }
}

/// Runtime-only song metadata displayed by the UI.
///
/// This deliberately lives outside [`SharedState`]'s serialized song data.
/// The storage task updates it after scans and completed operations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SongLibraryStatus {
    pub occupied: SongSlotOccupancy,
    pub current_slot: Option<SongSlot>,
    pub dirty: bool,
}

impl SongLibraryStatus {
    pub const fn empty() -> Self {
        Self {
            occupied: SongSlotOccupancy::empty(),
            current_slot: None,
            dirty: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SongMenuOperation {
    #[default]
    Load,
    SaveAs,
    Copy,
    Delete,
}

impl SongMenuOperation {
    pub const ALL: [Self; 4] = [Self::Load, Self::SaveAs, Self::Copy, Self::Delete];

    const fn index(self) -> usize {
        match self {
            Self::Load => 0,
            Self::SaveAs => 1,
            Self::Copy => 2,
            Self::Delete => 3,
        }
    }

    fn adjusted(self, delta: i32) -> Self {
        let index = (self.index() as i32)
            .saturating_add(delta)
            .clamp(0, Self::ALL.len() as i32 - 1) as usize;
        Self::ALL[index]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SongBrowserPurpose {
    Load,
    SaveAs,
    CopySource,
    CopyDestination { source: SongSlot },
    Delete,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SongConfirmChoice {
    #[default]
    Cancel,
    Confirm,
}

impl SongConfirmChoice {
    fn adjusted(self, delta: i32) -> Self {
        let current = i32::from(self == Self::Confirm);
        if current.saturating_add(delta).clamp(0, 1) == 0 {
            Self::Cancel
        } else {
            Self::Confirm
        }
    }
}

/// A complete flash operation request emitted by the pure UI controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SongStorageOperation {
    Format,
    SaveCurrent,
    Load {
        slot: SongSlot,
    },
    SaveAs {
        slot: SongSlot,
    },
    Copy {
        source: SongSlot,
        destination: SongSlot,
    },
    Delete {
        slot: SongSlot,
    },
}

impl SongStorageOperation {
    pub const fn destination_slot(self) -> Option<SongSlot> {
        match self {
            Self::Format | Self::SaveCurrent => None,
            Self::Load { slot } | Self::SaveAs { slot } | Self::Delete { slot } => Some(slot),
            Self::Copy { destination, .. } => Some(destination),
        }
    }
}

/// Status overlay supplied by the storage runtime.
///
/// `UnsupportedVersion` is intentionally distinct from corruption so newer
/// firmware formats are never presented as damaged or silently discarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SongUiStatus {
    Busy {
        operation: SongStorageOperation,
    },
    Formatting {
        percent: u8,
    },
    Success {
        operation: SongStorageOperation,
    },
    /// The requested Save already matches the live song revision, or Copy
    /// selected the same source and destination. No flash write occurred.
    NoChanges {
        slot: SongSlot,
    },
    /// An operation requiring an existing source selected an empty slot.
    Empty {
        slot: SongSlot,
    },
    UnsupportedVersion {
        slot: Option<SongSlot>,
        found: u16,
        supported: u16,
    },
    /// The raw partition or sequential-storage layout is valid but newer (or
    /// older) than the backend supported by this firmware.
    UnsupportedStorage {
        found: u32,
        supported: u32,
    },
    Corrupt {
        slot: Option<SongSlot>,
    },
    /// A low-level flash or journal operation failed without proving that the
    /// existing data is corrupt. The operation may be retried after reboot.
    Failed {
        operation: SongStorageOperation,
    },
    /// The flash device could not be probed at boot, so no storage operation
    /// can be attempted safely in this run.
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SongsView {
    Operations {
        selected: SongMenuOperation,
    },
    Browser {
        purpose: SongBrowserPurpose,
        slot: SongSlot,
    },
    Confirmation {
        operation: SongStorageOperation,
        choice: SongConfirmChoice,
    },
}

impl Default for SongsView {
    fn default() -> Self {
        Self::Operations {
            selected: SongMenuOperation::Load,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ResetAllChoice {
    #[default]
    Cancel,
    Reset,
}

impl ResetAllChoice {
    fn adjusted(self, delta: i32) -> Self {
        let current = i32::from(self == Self::Reset);
        if current.saturating_add(delta).clamp(0, 1) == 0 {
            Self::Cancel
        } else {
            Self::Reset
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiEncoderTarget {
    Volume(VolumeTarget),
    PatternVolume(PatternVolumeTarget),
    Root,
    Pattern(usize),
    PatternAll(usize),
    PatternNone,
    BeatsGlobal,
    BeatsPad(usize),
    Sample(Option<usize>),
    Light,
    Songs,
    SongStatus,
    ResetAll,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiAction {
    Pattern(PatternEditorAction),
    Song(SongStorageOperation),
    ResetConfirmed,
}

/// Complete, value-independent OLED route selected by the UI controller.
///
/// Musical values (division, sample, brightness, and volume) are resolved from
/// `SharedState` by the firmware after this pure model has selected the screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiDisplayModel {
    Root {
        highlighted: RootMode,
        selected_pad: Option<usize>,
        current_song: Option<SongSlot>,
        song_dirty: bool,
    },
    Volume {
        target: VolumeTarget,
    },
    PatternVolume {
        target: PatternVolumeTarget,
    },
    PatternSelectVoice,
    PatternEditor {
        pad: usize,
        cursor: u16,
    },
    PatternAll {
        pad: usize,
        choice: PatternAllChoice,
    },
    BeatsGlobal,
    BeatsPad {
        pad: usize,
    },
    SampleSelectVoice,
    SamplePad {
        pad: usize,
    },
    Light,
    SongsMenu {
        selected: SongMenuOperation,
    },
    SongBrowser {
        purpose: SongBrowserPurpose,
        slot: SongSlot,
        occupied: SongSlotOccupancy,
    },
    SongConfirmation {
        operation: SongStorageOperation,
        choice: SongConfirmChoice,
        /// Whether Save-as or Copy will replace an existing destination.
        destination_occupied: bool,
        /// Whether Load will discard edits made since the current song was
        /// saved or loaded.
        live_song_dirty: bool,
    },
    SongStatus {
        status: SongUiStatus,
    },
    ResetAll {
        choice: ResetAllChoice,
    },
}

/// Pure UI navigation model shared by controls, LEDs, OLED rendering, and
/// host tests. Physical debounce and shared musical data remain outside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UiController {
    page: UiPage,
    root_mode: RootMode,
    selection: VoiceSelection,
    pattern_cursors: [u16; BEAT_PAD_COUNT],
    pattern_all_menu: Option<PatternAllMenu>,
    reset_choice: ResetAllChoice,
    songs_view: SongsView,
    song_status: Option<SongUiStatus>,
    suppressed_keys: u16,
    encoder_suppressed: bool,
}

impl UiController {
    pub const fn new() -> Self {
        Self {
            page: UiPage::Root,
            root_mode: RootMode::Beats,
            selection: VoiceSelection::new(),
            pattern_cursors: [0; BEAT_PAD_COUNT],
            pattern_all_menu: None,
            reset_choice: ResetAllChoice::Cancel,
            songs_view: SongsView::Operations {
                selected: SongMenuOperation::Load,
            },
            song_status: None,
            suppressed_keys: 0,
            encoder_suppressed: false,
        }
    }

    pub const fn page(self) -> UiPage {
        self.page
    }

    pub const fn root_mode(self) -> RootMode {
        self.root_mode
    }

    pub const fn selection(self) -> VoiceSelection {
        self.selection
    }

    pub const fn selected_pad(self) -> Option<usize> {
        self.selection.selected()
    }

    pub fn press_voice(&mut self, pad: usize) -> bool {
        let before = self.selection;
        if !self.selection.toggle_exclusive(pad) {
            return false;
        }
        if self.selection != before {
            self.pattern_all_menu = None;
        }
        true
    }

    pub fn rotate_root(&mut self, delta: i32) {
        if self.page == UiPage::Root {
            self.root_mode = self.root_mode.clamped_offset(delta);
        }
    }

    pub fn enter_root_mode(&mut self) {
        if self.page != UiPage::Root {
            return;
        }
        self.pattern_all_menu = None;
        self.reset_choice = ResetAllChoice::Cancel;
        self.page = match self.root_mode {
            RootMode::Pattern => UiPage::Pattern,
            RootMode::Beats => UiPage::Beats,
            RootMode::Sample => UiPage::Sample,
            RootMode::Light => UiPage::Light,
            RootMode::Save => UiPage::Root,
            RootMode::Songs => UiPage::Songs,
            RootMode::ResetAll => UiPage::ResetAll,
        };
    }

    pub const fn pattern_cursor(self, pad: usize) -> Option<u16> {
        if pad < BEAT_PAD_COUNT {
            Some(self.pattern_cursors[pad])
        } else {
            None
        }
    }

    pub const fn pattern_all_menu(self) -> Option<PatternAllMenu> {
        self.pattern_all_menu
    }

    pub const fn reset_choice(self) -> ResetAllChoice {
        self.reset_choice
    }

    pub const fn songs_view(self) -> SongsView {
        self.songs_view
    }

    pub const fn song_status(self) -> Option<SongUiStatus> {
        self.song_status
    }

    /// Replace the storage overlay after an operation or boot scan completes.
    pub fn set_song_status(&mut self, status: SongUiStatus) {
        self.song_status = Some(status);
    }

    pub fn clear_song_status(&mut self) {
        self.song_status = None;
    }

    /// Resolve root-level Save when there is no current slot.
    pub fn open_save_as(&mut self, initial_slot: Option<SongSlot>) {
        self.song_status = None;
        self.page = UiPage::Songs;
        self.songs_view = SongsView::Browser {
            purpose: SongBrowserPurpose::SaveAs,
            slot: initial_slot.unwrap_or_default(),
        };
    }

    pub fn clamp_pattern_cursor(&mut self, pad: usize, division: u16) -> bool {
        let Some(cursor) = self.pattern_cursors.get_mut(pad) else {
            return false;
        };
        *cursor = (*cursor).min(division.min(MAX_BEAT_MULTIPLIER));
        true
    }

    /// Clamp every remembered Pattern cursor after replacing the live song.
    ///
    /// Selection and per-pad cursor memory survive Load, but a loaded song can
    /// expose fewer trigger rows than the previous one. Clamping all pads here
    /// prevents a later voice switch or Volume overlay from targeting a hidden
    /// row before the first encoder movement has a chance to clamp it.
    pub fn clamp_pattern_cursors(&mut self, divisions: &[u16; BEAT_PAD_COUNT]) {
        for (pad, &division) in divisions.iter().enumerate() {
            let _ = self.clamp_pattern_cursor(pad, division);
        }
    }

    pub fn rotate_pattern(&mut self, pad: usize, division: u16, delta: i32) {
        if self.page != UiPage::Pattern || Some(pad) != self.selected_pad() {
            return;
        }
        if let Some(menu) = &mut self.pattern_all_menu {
            if menu.pad == pad {
                menu.choice = menu.choice.adjusted(delta);
            }
            return;
        }
        if self.clamp_pattern_cursor(pad, division) {
            self.pattern_cursors[pad] =
                adjust_pattern_cursor(self.pattern_cursors[pad], division, delta);
        }
    }

    pub fn rotate_reset_choice(&mut self, delta: i32) {
        if self.page == UiPage::ResetAll {
            self.reset_choice = self.reset_choice.adjusted(delta);
        }
    }

    pub fn rotate_songs(&mut self, delta: i32) {
        if self.page != UiPage::Songs || self.song_status.is_some() {
            return;
        }
        self.songs_view = match self.songs_view {
            SongsView::Operations { selected } => SongsView::Operations {
                selected: selected.adjusted(delta),
            },
            SongsView::Browser { purpose, slot } => {
                let index = (slot.index() as i32)
                    .saturating_add(delta)
                    .clamp(0, SONG_SLOT_COUNT as i32 - 1) as usize;
                SongsView::Browser {
                    purpose,
                    slot: SongSlot::from_index(index).unwrap_or_default(),
                }
            }
            SongsView::Confirmation { operation, choice } => SongsView::Confirmation {
                operation,
                choice: choice.adjusted(delta),
            },
        };
    }

    pub fn press_encoder(&mut self, selected_division: Option<u16>) -> Option<UiAction> {
        if let Some(status) = self.song_status {
            if matches!(status, SongUiStatus::UnsupportedStorage { .. }) {
                self.song_status = None;
                self.page = UiPage::Songs;
                self.songs_view = SongsView::Confirmation {
                    operation: SongStorageOperation::Format,
                    choice: SongConfirmChoice::Cancel,
                };
                return None;
            }
            if !matches!(
                status,
                SongUiStatus::Busy { .. } | SongUiStatus::Formatting { .. }
            ) {
                self.song_status = None;
            }
            return None;
        }
        match self.page {
            UiPage::Root => {
                if self.root_mode == RootMode::Save {
                    Some(self.begin_song_operation(SongStorageOperation::SaveCurrent))
                } else {
                    self.enter_root_mode();
                    None
                }
            }
            UiPage::Pattern => self.press_pattern_control(selected_division),
            UiPage::Songs => self.press_songs_control(),
            UiPage::ResetAll => {
                let confirmed = self.reset_choice == ResetAllChoice::Reset;
                self.page = UiPage::Root;
                self.reset_choice = ResetAllChoice::Cancel;
                if confirmed {
                    self.selection.clear();
                    Some(UiAction::ResetConfirmed)
                } else {
                    None
                }
            }
            UiPage::Beats | UiPage::Sample | UiPage::Light => None,
        }
    }

    fn press_songs_control(&mut self) -> Option<UiAction> {
        match self.songs_view {
            SongsView::Operations { selected } => {
                let purpose = match selected {
                    SongMenuOperation::Load => SongBrowserPurpose::Load,
                    SongMenuOperation::SaveAs => SongBrowserPurpose::SaveAs,
                    SongMenuOperation::Copy => SongBrowserPurpose::CopySource,
                    SongMenuOperation::Delete => SongBrowserPurpose::Delete,
                };
                self.songs_view = SongsView::Browser {
                    purpose,
                    slot: SongSlot::default(),
                };
                None
            }
            SongsView::Browser { purpose, slot } => {
                if purpose == SongBrowserPurpose::CopySource {
                    self.songs_view = SongsView::Browser {
                        purpose: SongBrowserPurpose::CopyDestination { source: slot },
                        slot,
                    };
                } else {
                    let operation = match purpose {
                        SongBrowserPurpose::Load => SongStorageOperation::Load { slot },
                        SongBrowserPurpose::SaveAs => SongStorageOperation::SaveAs { slot },
                        SongBrowserPurpose::CopyDestination { source } => {
                            SongStorageOperation::Copy {
                                source,
                                destination: slot,
                            }
                        }
                        SongBrowserPurpose::Delete => SongStorageOperation::Delete { slot },
                        SongBrowserPurpose::CopySource => unreachable!(),
                    };
                    self.songs_view = SongsView::Confirmation {
                        operation,
                        choice: SongConfirmChoice::Cancel,
                    };
                }
                None
            }
            SongsView::Confirmation { operation, choice } => {
                if choice == SongConfirmChoice::Confirm {
                    let selected = match operation {
                        SongStorageOperation::Format => SongMenuOperation::Load,
                        SongStorageOperation::SaveCurrent | SongStorageOperation::SaveAs { .. } => {
                            SongMenuOperation::SaveAs
                        }
                        SongStorageOperation::Load { .. } => SongMenuOperation::Load,
                        SongStorageOperation::Copy { .. } => SongMenuOperation::Copy,
                        SongStorageOperation::Delete { .. } => SongMenuOperation::Delete,
                    };
                    self.songs_view = SongsView::Operations { selected };
                    Some(self.begin_song_operation(operation))
                } else {
                    self.songs_view = Self::browser_for_operation(operation);
                    None
                }
            }
        }
    }

    fn begin_song_operation(&mut self, operation: SongStorageOperation) -> UiAction {
        self.song_status = Some(SongUiStatus::Busy { operation });
        UiAction::Song(operation)
    }

    fn browser_for_operation(operation: SongStorageOperation) -> SongsView {
        match operation {
            SongStorageOperation::Format => SongsView::Operations {
                selected: SongMenuOperation::Load,
            },
            SongStorageOperation::SaveCurrent => SongsView::Operations {
                selected: SongMenuOperation::SaveAs,
            },
            SongStorageOperation::Load { slot } => SongsView::Browser {
                purpose: SongBrowserPurpose::Load,
                slot,
            },
            SongStorageOperation::SaveAs { slot } => SongsView::Browser {
                purpose: SongBrowserPurpose::SaveAs,
                slot,
            },
            SongStorageOperation::Copy {
                source,
                destination,
            } => SongsView::Browser {
                purpose: SongBrowserPurpose::CopyDestination { source },
                slot: destination,
            },
            SongStorageOperation::Delete { slot } => SongsView::Browser {
                purpose: SongBrowserPurpose::Delete,
                slot,
            },
        }
    }

    /// Activate the highlighted Pattern row from the encoder push without
    /// applying any behavior on other pages.
    pub fn press_pattern_control(&mut self, selected_division: Option<u16>) -> Option<UiAction> {
        if self.page != UiPage::Pattern {
            return None;
        }
        let pad = self.selected_pad()?;
        let division = selected_division?.min(MAX_BEAT_MULTIPLIER);
        self.clamp_pattern_cursor(pad, division);
        if let Some(menu) = self.pattern_all_menu.take() {
            let action = if menu.pad != pad || menu.choice == PatternAllChoice::Cancel {
                PatternEditorAction::AllMenuCancelled
            } else {
                PatternEditorAction::SetAll {
                    pad,
                    enabled: menu.choice == PatternAllChoice::All,
                }
            };
            return Some(UiAction::Pattern(action));
        }
        let cursor = self.pattern_cursors[pad];
        if cursor == 0 {
            self.pattern_all_menu = Some(PatternAllMenu {
                pad,
                choice: PatternAllChoice::Cancel,
            });
            Some(UiAction::Pattern(PatternEditorAction::AllMenuOpened))
        } else {
            Some(UiAction::Pattern(PatternEditorAction::Toggle {
                pad,
                step: cursor - 1,
            }))
        }
    }

    pub fn encoder_target(self, volume_pressed: bool) -> UiEncoderTarget {
        if self.song_status.is_some() {
            return UiEncoderTarget::SongStatus;
        }
        if volume_pressed {
            if self.page == UiPage::Pattern
                && let Some(target) = self.highlighted_pattern_volume_target()
            {
                return UiEncoderTarget::PatternVolume(target);
            }
            return UiEncoderTarget::Volume(VolumeTarget::for_selected_pad(self.selected_pad()));
        }
        match self.page {
            UiPage::Root => UiEncoderTarget::Root,
            UiPage::Pattern => match self.selected_pad() {
                Some(pad) if self.pattern_all_menu.is_some() => UiEncoderTarget::PatternAll(pad),
                Some(pad) => UiEncoderTarget::Pattern(pad),
                None => UiEncoderTarget::PatternNone,
            },
            UiPage::Beats => match self.selected_pad() {
                Some(pad) => UiEncoderTarget::BeatsPad(pad),
                None => UiEncoderTarget::BeatsGlobal,
            },
            UiPage::Sample => UiEncoderTarget::Sample(self.selected_pad()),
            UiPage::Light => UiEncoderTarget::Light,
            UiPage::Songs => UiEncoderTarget::Songs,
            UiPage::ResetAll => UiEncoderTarget::ResetAll,
        }
    }

    pub fn display_model(self, volume_pressed: bool) -> UiDisplayModel {
        self.display_model_with_library(volume_pressed, SongLibraryStatus::empty())
    }

    pub fn display_model_with_library(
        self,
        volume_pressed: bool,
        library: SongLibraryStatus,
    ) -> UiDisplayModel {
        if let Some(status) = self.song_status {
            return UiDisplayModel::SongStatus { status };
        }
        if volume_pressed {
            if self.page == UiPage::Pattern
                && let Some(target) = self.highlighted_pattern_volume_target()
            {
                return UiDisplayModel::PatternVolume { target };
            }
            return UiDisplayModel::Volume {
                target: VolumeTarget::for_selected_pad(self.selected_pad()),
            };
        }
        match self.page {
            UiPage::Root => UiDisplayModel::Root {
                highlighted: self.root_mode,
                selected_pad: self.selected_pad(),
                current_song: library.current_slot,
                song_dirty: library.dirty,
            },
            UiPage::Pattern => match self.selected_pad() {
                Some(pad) => match self.pattern_all_menu {
                    Some(menu) => UiDisplayModel::PatternAll {
                        pad,
                        choice: menu.choice,
                    },
                    None => UiDisplayModel::PatternEditor {
                        pad,
                        cursor: self.pattern_cursors[pad],
                    },
                },
                None => UiDisplayModel::PatternSelectVoice,
            },
            UiPage::Beats => match self.selected_pad() {
                Some(pad) => UiDisplayModel::BeatsPad { pad },
                None => UiDisplayModel::BeatsGlobal,
            },
            UiPage::Sample => match self.selected_pad() {
                Some(pad) => UiDisplayModel::SamplePad { pad },
                None => UiDisplayModel::SampleSelectVoice,
            },
            UiPage::Light => UiDisplayModel::Light,
            UiPage::Songs => match self.songs_view {
                SongsView::Operations { selected } => UiDisplayModel::SongsMenu { selected },
                SongsView::Browser { purpose, slot } => UiDisplayModel::SongBrowser {
                    purpose,
                    slot,
                    occupied: library.occupied,
                },
                SongsView::Confirmation { operation, choice } => {
                    let destination_occupied = match operation {
                        SongStorageOperation::SaveAs { slot } => library.occupied.contains(slot),
                        SongStorageOperation::Copy { destination, .. } => {
                            library.occupied.contains(destination)
                        }
                        SongStorageOperation::SaveCurrent
                        | SongStorageOperation::Format
                        | SongStorageOperation::Load { .. }
                        | SongStorageOperation::Delete { .. } => false,
                    };
                    UiDisplayModel::SongConfirmation {
                        operation,
                        choice,
                        destination_occupied,
                        live_song_dirty: library.dirty,
                    }
                }
            },
            UiPage::ResetAll => UiDisplayModel::ResetAll {
                choice: self.reset_choice,
            },
        }
    }

    const fn highlighted_pattern_volume_target(self) -> Option<PatternVolumeTarget> {
        let Some(pad) = self.selected_pad() else {
            return None;
        };
        let cursor = self.pattern_cursors[pad];
        if cursor == 0 {
            Some(PatternVolumeTarget::All { pad })
        } else {
            Some(PatternVolumeTarget::Step {
                pad,
                step: cursor - 1,
            })
        }
    }

    /// Return directly to the root while remembering its cursor. Leaving a
    /// mode or confirmation preserves voice selection; pressing Return while
    /// already at the root clears it. Every key or encoder push already held
    /// is suppressed until its release.
    pub fn return_to_root(&mut self, held_keys: u16, encoder_held: bool) {
        let clear_selection = self.page == UiPage::Root;
        self.page = UiPage::Root;
        if clear_selection {
            self.selection.clear();
        }
        self.pattern_all_menu = None;
        self.reset_choice = ResetAllChoice::Cancel;
        let selected_song_operation = match self.songs_view {
            SongsView::Operations { selected } => selected,
            SongsView::Browser { purpose, .. } => match purpose {
                SongBrowserPurpose::Load => SongMenuOperation::Load,
                SongBrowserPurpose::SaveAs => SongMenuOperation::SaveAs,
                SongBrowserPurpose::CopySource | SongBrowserPurpose::CopyDestination { .. } => {
                    SongMenuOperation::Copy
                }
                SongBrowserPurpose::Delete => SongMenuOperation::Delete,
            },
            SongsView::Confirmation { operation, .. } => match operation {
                SongStorageOperation::Format => SongMenuOperation::Load,
                SongStorageOperation::SaveCurrent | SongStorageOperation::SaveAs { .. } => {
                    SongMenuOperation::SaveAs
                }
                SongStorageOperation::Load { .. } => SongMenuOperation::Load,
                SongStorageOperation::Copy { .. } => SongMenuOperation::Copy,
                SongStorageOperation::Delete { .. } => SongMenuOperation::Delete,
            },
        };
        self.songs_view = SongsView::Operations {
            selected: selected_song_operation,
        };
        if !matches!(
            self.song_status,
            Some(SongUiStatus::Busy { .. } | SongUiStatus::Formatting { .. })
        ) {
            self.song_status = None;
        }
        self.suppressed_keys = held_keys & ((1_u16 << KEY_COUNT) - 1);
        self.encoder_suppressed = encoder_held;
    }

    pub fn update_suppression(&mut self, held_keys: u16, encoder_held: bool) {
        self.suppressed_keys &= held_keys;
        self.encoder_suppressed &= encoder_held;
    }

    pub const fn suppressed_keys(self) -> u16 {
        self.suppressed_keys
    }

    pub const fn key_suppressed(self, key: usize) -> bool {
        key < KEY_COUNT && self.suppressed_keys & (1_u16 << key) != 0
    }

    pub const fn encoder_suppressed(self) -> bool {
        self.encoder_suppressed
    }
}

impl Default for UiController {
    fn default() -> Self {
        Self::new()
    }
}

/// Debounces twelve active-high logical key bits using consecutive samples.
pub struct KeyDebouncer {
    threshold: u8,
    stable_mask: u16,
    counters: [u8; KEY_COUNT],
}

impl KeyDebouncer {
    pub fn new(stable_samples: u8) -> Self {
        Self {
            threshold: stable_samples.max(1),
            stable_mask: 0,
            counters: [0; KEY_COUNT],
        }
    }

    pub fn stable_mask(&self) -> u16 {
        self.stable_mask
    }

    pub fn update(&mut self, raw_mask: u16) -> KeyChanges {
        let mut changes = KeyChanges::default();
        for key in 0..KEY_COUNT {
            let bit = 1_u16 << key;
            let raw = raw_mask & bit != 0;
            let stable = self.stable_mask & bit != 0;
            if raw == stable {
                self.counters[key] = 0;
                continue;
            }

            self.counters[key] = self.counters[key].saturating_add(1);
            if self.counters[key] >= self.threshold {
                self.counters[key] = 0;
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

pub fn adjust_volume_percent(current_percent: u8, delta: i32) -> u8 {
    i32::from(current_percent.min(100))
        .saturating_add(delta)
        .clamp(0, 100) as u8
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternEditorAction {
    AllMenuOpened,
    AllMenuCancelled,
    SetAll { pad: usize, enabled: bool },
    Toggle { pad: usize, step: u16 },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PatternAllChoice {
    #[default]
    Cancel,
    All,
    None,
}

impl PatternAllChoice {
    fn adjusted(self, delta: i32) -> Self {
        let index: i32 = match self {
            Self::Cancel => 0,
            Self::All => 1,
            Self::None => 2,
        };
        match index.saturating_add(delta).clamp(0, 2) {
            0 => Self::Cancel,
            1 => Self::All,
            _ => Self::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PatternAllMenu {
    pub pad: usize,
    pub choice: PatternAllChoice,
}

pub fn adjust_pattern_cursor(cursor: u16, division: u16, delta: i32) -> u16 {
    i32::from(cursor.min(division))
        .saturating_add(delta)
        .clamp(0, i32::from(division)) as u16
}

pub fn pattern_window_start(cursor: u16, division: u16, visible_rows: u16) -> u16 {
    let entry_count = division.saturating_add(1);
    if visible_rows == 0 || entry_count <= visible_rows {
        return 0;
    }
    cursor
        .saturating_sub(visible_rows / 2)
        .min(entry_count - visible_rows)
}

/// A selectable slice plus continuation flags for a scrolling OLED menu.
/// Firmware renders the flags beside the first and last item, so they consume
/// horizontal marker space rather than reducing the number of visible items.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScrollMenuWindow {
    pub start: usize,
    pub item_rows: usize,
    pub more_above: bool,
    pub more_below: bool,
}

pub fn scroll_menu_window(
    selected: usize,
    item_count: usize,
    visible_rows: usize,
) -> ScrollMenuWindow {
    if item_count == 0 || visible_rows == 0 {
        return ScrollMenuWindow::default();
    }
    let selected = selected.min(item_count - 1);
    let item_rows = item_count.min(visible_rows);
    // When more items remain below, keep the selection one row above the
    // bottom so the final row can carry the continuation triangle in the
    // ordinary selection-marker column. Each further selection still shifts
    // the window by exactly one entry.
    let start = selected
        .saturating_sub(item_rows.saturating_sub(2))
        .min(item_count - item_rows);
    ScrollMenuWindow {
        start,
        item_rows,
        more_above: start != 0,
        more_below: start + item_rows < item_count,
    }
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

#[derive(Clone, Copy, Debug, Default)]
pub struct UiEncoderAcceleration {
    last_event: Option<(u64, UiEncoderTarget, i32)>,
}

impl UiEncoderAcceleration {
    pub const fn new() -> Self {
        Self { last_event: None }
    }

    pub fn update(&mut self, now_ms: u64, target: UiEncoderTarget, direction: i32) -> i32 {
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

pub fn scale_color(color: (u8, u8, u8), brightness_percent: u8) -> (u8, u8, u8) {
    let brightness = u16::from(brightness_percent.min(100));
    let scale = |channel: u8| ((u16::from(channel) * brightness + 50) / 100) as u8;
    (scale(color.0), scale(color.1), scale(color.2))
}

/// Red mute-control LED, scaled by the configured brightness. Active mute is
/// shown at a fixed fraction of the unmuted red level.
pub fn mute_led_color(muted: bool, brightness_percent: u8) -> (u8, u8, u8) {
    let brightness = brightness_percent.min(100);
    let brightness = if muted {
        ((u16::from(brightness) * u16::from(MUTE_LED_DIM_PERCENT) + 50) / 100) as u8
    } else {
        brightness
    };
    scale_color((u8::MAX, 0, 0), brightness)
}

/// Yellow volume-control LED. Its level combines the selected stored volume
/// with the configured LED brightness.
pub fn volume_led_color(volume_percent: u8, brightness_percent: u8) -> (u8, u8, u8) {
    let combined_percent =
        (u16::from(volume_percent.min(100)) * u16::from(brightness_percent.min(100)) + 50) / 100;
    scale_color((u8::MAX, u8::MAX, 0), combined_percent as u8)
}

/// White Return-control LED, scaled only by the configured key brightness.
pub fn return_led_color(brightness_percent: u8) -> (u8, u8, u8) {
    scale_color((u8::MAX, u8::MAX, u8::MAX), brightness_percent)
}

/// Apply an additional 80% dimming layer to an already-computed beat-key color.
/// An idle-off key therefore remains off.
pub fn dim_nonselected_led_color(
    color: (u8, u8, u8),
    selection_active: bool,
    selected: bool,
) -> (u8, u8, u8) {
    if selection_active && !selected {
        scale_color(color, NONSELECTED_LED_SCALE_PERCENT)
    } else {
        color
    }
}

/// Compose voice palette, selection/trigger state, and configured brightness.
pub fn voice_led_color(
    pad: usize,
    brightness_percent: u8,
    selection_active: bool,
    selected: bool,
    trigger_active: bool,
    light_preview: bool,
) -> (u8, u8, u8) {
    let palette = colorwheel((21 * pad) as u8);
    let base_color = if trigger_active {
        palette
    } else if selected {
        (u8::MAX, u8::MAX, u8::MAX)
    } else if light_preview {
        palette
    } else {
        (0, 0, 0)
    };
    let configured = scale_color(base_color, brightness_percent);
    dim_nonselected_led_color(configured, selection_active, selected)
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
    const DENSE_TEST_DIVISION: u16 = MAX_BEAT_MULTIPLIER;
    const DENSE_TEST_INTERVAL_MS: u32 = MAX_BEAT_MULTIPLIER as u32;
    static TEST_SAMPLE_NAMES: [&str; SAMPLE_COUNT] = ["Test"; SAMPLE_COUNT];

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

    fn test_catalog<'a>(kick_bytes: &'a [u8], hat_bytes: &'a [u8]) -> SampleCatalog<'a> {
        let kick = WavPcm16::parse(kick_bytes).unwrap();
        let hat = WavPcm16::parse(hat_bytes).unwrap();
        let mut samples = [kick; SAMPLE_COUNT];
        samples[DEFAULT_OPEN_HAT_SAMPLE.index()] = hat;
        SampleCatalog::new(samples, &TEST_SAMPLE_NAMES)
    }

    fn test_sequencer<'a>(kick_bytes: &'a [u8], hat_bytes: &'a [u8]) -> Sequencer<'a> {
        Sequencer::new(test_catalog(kick_bytes, hat_bytes))
    }

    fn sample(index: usize) -> SampleId {
        SampleId::from_index(index).unwrap()
    }

    #[derive(Clone, Copy, Debug)]
    struct ReferencePadClock {
        division: u16,
        tick_ordinal: u128,
        next_frame: u64,
    }

    impl ReferencePadClock {
        fn aligned(division: u16, base_interval_ms: u32, from_frame: u64) -> Self {
            let tick_ordinal = next_ordinal_after(from_frame, division, base_interval_ms);
            Self {
                division,
                tick_ordinal,
                next_frame: frame_for_tick(tick_ordinal, division, base_interval_ms),
            }
        }

        fn take_due(&mut self, frame: u64, base_interval_ms: u32, pattern: &Pattern) -> bool {
            let mut enabled = false;
            while frame_has_reached(frame, self.next_frame) {
                let step = self.tick_ordinal.wrapping_sub(1) % u128::from(self.division);
                enabled |= pattern
                    .step_enabled(step as u16, self.division)
                    .unwrap_or(false);
                self.tick_ordinal = self.tick_ordinal.wrapping_add(1);
                self.next_frame =
                    frame_for_tick(self.tick_ordinal, self.division, base_interval_ms);
            }
            enabled
        }
    }

    fn patterned_clock_fixture() -> Pattern {
        let mut pattern = Pattern::default();
        pattern.fill(false);
        for bit in 0..PATTERN_BITS {
            if bit % 7 == 1 || bit % 19 == 3 {
                assert!(pattern.set_bit(bit, true));
            }
        }
        pattern
    }

    #[test]
    fn parses_repository_samples() {
        let kick = WavPcm16::parse(KICK_WAV).unwrap();
        let hat = WavPcm16::parse(HAT_WAV).unwrap();
        assert_eq!(kick.len(), 11_265);
        assert_eq!(hat.len(), 6_852);
        assert!(kick.sample(0).is_some());
        assert_eq!(kick.sample(kick.len()), None);
        let catalog = sample_assets::parse_catalog().unwrap();
        assert!(catalog.samples().iter().all(|sample| !sample.is_empty()));

        let expected_names = [
            "909 Kick",
            "909 Snare",
            "909 Hat Closed",
            "909 Hat Open",
            "909 Clap",
            "909 Tom",
            "909 Blip",
            "909 Cymbal",
            "Tac Kick",
            "Tac Snare",
            "Tac Hat Closed",
            "Tac Hat Open",
            "Tac Snare Roll",
            "Tac Tom",
            "Tac Ride Bell",
            "Tac Cymbal",
            "AKU Kick",
            "AKU Snare",
            "AKU Hat 1",
            "AKU Hat 2",
            "AKU Clq",
            "AKU Pcq 06",
            "AKU Pcq 10",
            "AKU Cymbal",
        ];
        let expected_paths = [
            "kit0_909/00_909kick4.wav",
            "kit0_909/01_909snare2.wav",
            "kit0_909/02_909hatclosed2a.wav",
            "kit0_909/03_909hatopen5.wav",
            "kit0_909/04_909clap1.wav",
            "kit0_909/05_909tommed.wav",
            "kit0_909/06_909blip.wav",
            "kit0_909/07_909cym2.wav",
            "kit1_tac/00tictac_kick.wav",
            "kit1_tac/01tictac_snare.wav",
            "kit1_tac/02tictac_hatc2.wav",
            "kit1_tac/03tictac_hato3.wav",
            "kit1_tac/04tictac_snareroll.wav",
            "kit1_tac/05tictac_tomlight.wav",
            "kit1_tac/06tictac_ridebell.wav",
            "kit1_tac/07tictac_cymbal1.wav",
            "kit2_aku/00_kick02.wav",
            "kit2_aku/01_sd02.wav",
            "kit2_aku/02_ho02.wav",
            "kit2_aku/03_ho02.wav",
            "kit2_aku/04_clq02.wav",
            "kit2_aku/05_pcq06.wav",
            "kit2_aku/06_pcq10.wav",
            "kit2_aku/07_cyq01.wav",
        ];
        let expected_frames = [
            11_577, 13_024, 6_582, 13_230, 22_326, 8_720, 2_561, 19_845, 26_368, 25_024, 6_822,
            40_961, 25_088, 67_328, 31_420, 66_058, 11_265, 15_776, 6_852, 24_370, 11_773, 18_178,
            12_641, 44_165,
        ];
        assert_eq!(sample_assets::SAMPLE_NAMES, expected_names);
        assert_eq!(sample_assets::SAMPLE_PATHS, expected_paths);
        for index in 0..SAMPLE_COUNT {
            let id = sample(index);
            let definition = catalog.definition(id);
            assert_eq!(definition.id, id);
            assert_eq!(definition.short_name, expected_names[index]);
            assert_eq!(definition.pcm.len(), expected_frames[index]);
        }
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
    fn voice_starts_and_stops_at_sample_end() {
        let kick_bytes = wav(&[10, 20], false);
        let hat_bytes = wav(&[-10], false);
        let catalog = test_catalog(&kick_bytes, &hat_bytes);
        let mut voice = PlaybackVoice::idle();

        assert_eq!(voice.render(&catalog, 65_536), 0);
        voice.start(0, DEFAULT_KICK_SAMPLE, 1);
        assert_eq!(voice.render(&catalog, 65_536), 10);
        assert!(voice.is_active());
        assert_eq!(voice.render(&catalog, 65_536), 20);
        assert!(!voice.is_active());
        assert_eq!(voice.render(&catalog, 65_536), 0);
    }

    #[test]
    fn pads_map_to_samples_and_mixing_saturates() {
        let kick_bytes = wav(&[20_000], false);
        let hat_bytes = wav(&[-20_000], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);

        for pad in 0..6 {
            assert_eq!(sequencer.pads[pad].sample, DEFAULT_KICK_SAMPLE);
        }
        for pad in 6..BEAT_PAD_COUNT {
            assert_eq!(sequencer.pads[pad].sample, DEFAULT_OPEN_HAT_SAMPLE);
        }

        let mut report = RenderReport::default();
        let allocation = VoiceAllocationState::settled(100, &[100; BEAT_PAD_COUNT]);
        assert!(sequencer.voices.start(
            0,
            DEFAULT_KICK_SAMPLE,
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert!(sequencer.voices.start(
            1,
            DEFAULT_KICK_SAMPLE,
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert_eq!(sequencer.render_pcm_frame(0, &mut report), i16::MAX);
        assert_eq!(report.clipped_frame_count, 1);

        assert!(sequencer.voices.start(
            6,
            DEFAULT_OPEN_HAT_SAMPLE,
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert!(sequencer.voices.start(
            7,
            DEFAULT_OPEN_HAT_SAMPLE,
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert_eq!(sequencer.render_pcm_frame(1, &mut report), i16::MIN);
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
    fn coarse_dither_preserves_duty_count_and_cross_frame_error() {
        const BASE_MASK: u32 = (1 << PWM_COMMAND_BITS) - 1;
        const DITHER_MASK: u32 = ((1 << PWM_DITHER_CYCLES) - 1) << PWM_COMMAND_BITS;

        for starting_error in 0_u16..512 {
            for fraction in 0_i16..512 {
                // Quantized level 64 keeps every possible nine-bit fraction
                // inside the signed-i16 range.
                let sample = fraction;
                let mut full = DitherEncoder {
                    error: starting_error,
                };
                let mut coarse = full;
                let full_word = full.encode(sample);
                let coarse_word = coarse.encode_coarse(sample);
                assert_eq!(coarse_word & BASE_MASK, full_word & BASE_MASK);
                assert_eq!(
                    (coarse_word & DITHER_MASK).count_ones(),
                    (full_word & DITHER_MASK).count_ones()
                );
                assert_eq!(coarse.error(), full.error());
                assert_eq!(coarse_word >> 30, 0);
            }
        }
    }

    #[test]
    fn patterns_use_fixed_direct_slots_for_every_division() {
        assert_eq!(core::mem::size_of::<Pattern>(), PATTERN_BYTES);

        let pattern = Pattern::default();
        assert_eq!(pattern.fill_state(), PatternFillState::Full);
        assert_eq!(pattern.bit(0), Some(true));
        assert_eq!(pattern.bit(PATTERN_BITS - 1), Some(true));
        assert_eq!(pattern.bit(PATTERN_BITS), None);

        for division in 1..=MAX_BEAT_MULTIPLIER {
            for step in 0..division {
                assert_eq!(pattern_step_index(step, division), Some(usize::from(step)));
            }
        }
        assert_eq!(pattern_step_index(0, 0), None);
        assert_eq!(pattern_step_index(3, 3), None);
        assert_eq!(pattern_step_index(0, MAX_BEAT_MULTIPLIER + 1), None);
    }

    #[test]
    fn pattern_edits_change_one_slot_and_hidden_slots_survive_resize() {
        let mut pattern = Pattern::default();
        assert_eq!(pattern.toggle_step(0, 2), Some(false));

        // Expanding exposes later stored slots without deriving them from the
        // earlier, smaller division.
        assert_eq!(pattern.step_enabled(0, 4), Some(false));
        assert_eq!(pattern.step_enabled(1, 4), Some(true));
        assert_eq!(pattern.step_enabled(2, 4), Some(true));
        assert_eq!(pattern.step_enabled(3, 4), Some(true));
        assert_eq!(pattern.bit(1), Some(true));
        assert_eq!(pattern.bit(PATTERN_BITS - 1), Some(true));

        // Slot 6 becomes hidden at division 4, but neither the division nor an
        // edit to a visible slot clears it.
        assert!(pattern.set_step_enabled(6, 8, false));
        assert!(pattern.set_step_enabled(1, 4, false));
        assert_eq!(pattern.step_enabled(1, 4), Some(false));
        assert_eq!(pattern.step_enabled(6, 4), None);
        assert_eq!(pattern.step_enabled(6, 8), Some(false));
        assert_eq!(pattern.bit(5), Some(true));
        assert_eq!(pattern.bit(7), Some(true));
    }

    #[test]
    fn whole_map_state_tracks_explicit_fill_and_clear() {
        let mut pattern = Pattern::default();
        assert_eq!(pattern.fill_state(), PatternFillState::Full);

        assert!(pattern.set_bit(PATTERN_BITS - 1, false));
        assert_eq!(pattern.fill_state(), PatternFillState::Mixed);
        pattern.fill(true);
        assert_eq!(pattern.fill_state(), PatternFillState::Full);
        assert_eq!(pattern.bit(PATTERN_BITS - 1), Some(true));

        pattern.fill(false);
        assert_eq!(pattern.fill_state(), PatternFillState::Empty);
        assert_eq!(pattern.bit(0), Some(false));
        assert_eq!(pattern.bit(PATTERN_BITS - 1), Some(false));
    }

    #[test]
    fn trigger_volumes_are_direct_persistent_slots_with_relative_all_edits() {
        let mut volumes = TriggerVolumes::default();
        assert_eq!(volumes.percent(0), Some(100));
        assert_eq!(volumes.percent(PATTERN_BITS - 1), Some(100));
        assert_eq!(volumes.percent(PATTERN_BITS), None);
        assert_eq!(volumes.average_percent(), 100);

        assert!(volumes.adjust_all(-20));
        assert_eq!(volumes.percent(0), Some(80));
        assert_eq!(volumes.percent(PATTERN_BITS - 1), Some(80));
        assert_eq!(volumes.average_percent(), 80);

        // The requested workflow: lower the complete map by 20 points, then
        // restore one accent by 10 points without changing any other slot.
        assert_eq!(volumes.adjust_step(7, 10), Some(90));
        assert_eq!(volumes.percent(7), Some(90));
        assert_eq!(volumes.percent(6), Some(80));
        assert_eq!(volumes.percent(8), Some(80));

        assert_eq!(volumes.adjust_step(PATTERN_BITS, 1), None);
        assert!(volumes.adjust_all(-1_000));
        assert_eq!(volumes.average_percent(), 0);
        assert!(!volumes.adjust_all(-1));
        assert!(volumes.adjust_all(1_000));
        assert_eq!(volumes.average_percent(), 100);
        assert!(!volumes.adjust_all(1));
    }

    #[test]
    fn shared_pattern_volume_edits_persist_until_a_whole_map_choice() {
        let mut state = SharedState::default();
        state.desired_beats[2] = 8;

        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::All { pad: 2 }, -20),
            Some(80)
        );
        assert_eq!(state.pattern_revision, 1);
        assert_eq!(state.take_pattern_dirty_mask(), 1 << 2);
        assert_eq!(state.trigger_volume(2, 0), Some(80));
        assert_eq!(state.trigger_volume(2, PATTERN_BITS - 1), Some(80));

        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::Step { pad: 2, step: 3 }, 10),
            Some(90)
        );
        assert_eq!(state.pattern_revision, 2);
        assert_eq!(state.take_pattern_dirty_mask(), 1 << 2);
        assert_eq!(state.trigger_volume(2, 3), Some(90));
        assert_eq!(state.trigger_volume(2, 4), Some(80));

        // Hidden rows cannot be edited individually, while shrinking and
        // toggling an individual enable bit preserve every stored level.
        state.desired_beats[2] = 4;
        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::Step { pad: 2, step: 4 }, 10),
            None
        );
        assert_eq!(state.trigger_volume(2, 4), Some(80));
        assert_eq!(state.toggle_pattern_step(2, 3), Some(false));
        assert_eq!(state.trigger_volume(2, 3), Some(90));
        assert_eq!(state.trigger_volume(2, PATTERN_BITS - 1), Some(80));

        // An explicit whole-map choice resets visible and hidden trigger
        // levels to the predictable 100% baseline.
        assert!(state.set_pattern_all(2, false));
        assert_eq!(state.trigger_volume(2, 3), Some(100));
        assert_eq!(state.trigger_volume(2, PATTERN_BITS - 1), Some(100));

        // Whole-map volume adjustment remains valid at division zero and
        // modifies hidden storage until the next All/None choice.
        state.desired_beats[2] = 0;
        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::All { pad: 2 }, -10),
            Some(90)
        );
        assert_eq!(state.trigger_volume(2, 3), Some(90));
        assert_eq!(state.trigger_volume(2, PATTERN_BITS - 1), Some(90));
        assert_eq!(
            state.adjust_pattern_volume(
                PatternVolumeTarget::All {
                    pad: BEAT_PAD_COUNT,
                },
                -1,
            ),
            None
        );
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

        // Explicit All/None choices are independent of division, cover all
        // 256 stored bits, and reset all trigger levels to 100%.
        state.desired_beats[2] = 0;
        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::All { pad: 2 }, -20),
            Some(80)
        );
        assert_eq!(state.pattern_revision, 2);
        assert_eq!(state.take_pattern_dirty_mask(), 1 << 2);
        assert!(state.set_pattern_all(2, true));
        assert_eq!(state.pattern_revision, 3);
        assert_eq!(
            state.pattern(2).unwrap().fill_state(),
            PatternFillState::Full
        );
        assert_eq!(state.trigger_volume(2, 0), Some(100));
        assert_eq!(state.trigger_volume(2, PATTERN_BITS - 1), Some(100));
        assert!((0..PATTERN_BITS).all(|step| state.trigger_volume(2, step) == Some(100)));
        assert_eq!(state.take_pattern_dirty_mask(), 1 << 2);

        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::All { pad: 2 }, -35),
            Some(65)
        );
        assert_eq!(state.pattern_revision, 4);
        assert_eq!(state.take_pattern_dirty_mask(), 1 << 2);
        assert!(state.set_pattern_all(2, false));
        assert_eq!(state.pattern_revision, 5);
        assert_eq!(
            state.pattern(2).unwrap().fill_state(),
            PatternFillState::Empty
        );
        assert_eq!(state.trigger_volume(2, 0), Some(100));
        assert_eq!(state.trigger_volume(2, PATTERN_BITS - 1), Some(100));
        assert!((0..PATTERN_BITS).all(|step| state.trigger_volume(2, step) == Some(100)));
        assert_eq!(state.take_pattern_dirty_mask(), 1 << 2);
        assert!(!state.set_pattern_all(BEAT_PAD_COUNT, true));
        assert_eq!(state.pattern_revision, 5);
    }

    #[test]
    fn rational_clock_matches_the_reference_ceil_grid() {
        let pattern = patterned_clock_fixture();
        let divisions = [1, 2, 3, 71, 73, 255, MAX_BEAT_MULTIPLIER];
        let base_intervals = [
            MIN_BASE_INTERVAL_MS,
            MIN_BASE_INTERVAL_MS + 1,
            999,
            DEFAULT_BASE_INTERVAL_MS,
            106_500,
            u32::MAX,
        ];
        let starts = [0, 12_345, u64::MAX - 96];

        for division in divisions {
            for base_interval_ms in base_intervals {
                for from_frame in starts {
                    let mut actual = PadState::new(0);
                    actual.beats_per_interval = division;
                    actual.align_clock(division, base_interval_ms, from_frame);
                    let mut reference =
                        ReferencePadClock::aligned(division, base_interval_ms, from_frame);

                    assert_eq!(actual.tick_ordinal, reference.tick_ordinal);
                    assert_eq!(actual.next_frame, Some(reference.next_frame));
                    for offset in 0..384_u64 {
                        let frame = from_frame.wrapping_add(offset);
                        assert_eq!(
                            actual.take_due(frame, &pattern),
                            reference.take_due(frame, base_interval_ms, &pattern),
                            "division={division}, base={base_interval_ms}, frame={frame}"
                        );
                        assert_eq!(actual.tick_ordinal, reference.tick_ordinal);
                        assert_eq!(actual.next_frame, Some(reference.next_frame));
                        assert!(actual.deadline_error < actual.period_denominator);
                        assert!(actual.next_step < division);
                        assert_eq!(
                            pattern_step_index(actual.next_step, division),
                            Some(usize::from(actual.next_step))
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn direct_pattern_cursor_matches_every_division() {
        let pattern = patterned_clock_fixture();

        for division in 1..=MAX_BEAT_MULTIPLIER {
            let mut clock = PadState::new(0);
            clock.beats_per_interval = division;
            clock.align_clock(division, DEFAULT_BASE_INTERVAL_MS, 0);

            for expected_step in 0..division {
                assert_eq!(clock.next_step, expected_step, "division={division}");
                assert_eq!(
                    clock.current_step_enabled(&pattern),
                    pattern.step_enabled(expected_step, division).unwrap(),
                    "division={division}, step={expected_step}"
                );
                clock.advance_one();
            }

            assert_eq!(clock.next_step, 0);
        }
    }

    #[test]
    fn division_256_uses_slot_255_then_wraps_to_slot_zero_with_its_own_gain() {
        let mut pattern = Pattern::default();
        pattern.fill(false);
        assert!(pattern.set_bit(0, true));
        assert!(pattern.set_bit(PATTERN_BITS - 1, true));
        let mut volumes = TriggerVolumes::default();
        assert!(volumes.adjust_all(-100));
        assert_eq!(volumes.adjust_step(0, 75), Some(75));
        assert_eq!(volumes.adjust_step(PATTERN_BITS - 1, 25), Some(25));

        let mut clock = PadState::new(0);
        clock.beats_per_interval = MAX_BEAT_MULTIPLIER;
        clock.align_clock(MAX_BEAT_MULTIPLIER, DENSE_TEST_INTERVAL_MS, 0);
        let through_wrap = frame_for_tick(
            u128::from(MAX_BEAT_MULTIPLIER) + 1,
            MAX_BEAT_MULTIPLIER,
            DENSE_TEST_INTERVAL_MS,
        );
        let mut events = Vec::new();
        for frame in 0..=through_wrap {
            if let Some(gain) = clock.take_due_trigger(frame, &pattern, &volumes) {
                events.push((frame, gain));
            }
        }

        assert_eq!(
            events,
            [
                (
                    frame_for_tick(1, MAX_BEAT_MULTIPLIER, DENSE_TEST_INTERVAL_MS),
                    75,
                ),
                (
                    frame_for_tick(
                        u128::from(MAX_BEAT_MULTIPLIER),
                        MAX_BEAT_MULTIPLIER,
                        DENSE_TEST_INTERVAL_MS,
                    ),
                    25,
                ),
                (through_wrap, 75),
            ]
        );
    }

    #[test]
    fn rational_clock_fast_forward_matches_reference_coalescing() {
        let pattern = patterned_clock_fixture();
        let division = MAX_BEAT_MULTIPLIER;
        let base_interval_ms = MIN_BASE_INTERVAL_MS;
        let mut actual = PadState::new(0);
        actual.beats_per_interval = division;
        actual.align_clock(division, base_interval_ms, 0);
        let mut reference = ReferencePadClock::aligned(division, base_interval_ms, 0);

        // Every jump after the first takes the bounded fast-forward path. The
        // longer gaps cross full pattern cycles and exercise cyclic OR logic.
        for frame in [0, 2, 127, 2_000, 10_000, 65_535] {
            assert_eq!(
                actual.take_due(frame, &pattern),
                reference.take_due(frame, base_interval_ms, &pattern),
                "frame={frame}"
            );
            assert_eq!(actual.tick_ordinal, reference.tick_ordinal);
            assert_eq!(actual.next_frame, Some(reference.next_frame));
        }
    }

    #[test]
    fn rational_clock_fast_forward_is_exact_across_frame_wrap() {
        let pattern = patterned_clock_fixture();
        let division = MAX_BEAT_MULTIPLIER;
        let base_interval_ms = MIN_BASE_INTERVAL_MS;
        let from_frame = u64::MAX - 1_000;
        let mut actual = PadState::new(0);
        actual.beats_per_interval = division;
        actual.align_clock(division, base_interval_ms, from_frame);
        let mut reference = ReferencePadClock::aligned(division, base_interval_ms, from_frame);

        for frame in [from_frame, u64::MAX - 10, 0, 1_024] {
            assert_eq!(
                actual.take_due(frame, &pattern),
                reference.take_due(frame, base_interval_ms, &pattern),
                "frame={frame}"
            );
            assert_eq!(actual.tick_ordinal, reference.tick_ordinal);
            assert_eq!(actual.next_frame, Some(reference.next_frame));
        }
    }

    #[test]
    fn scheduling_is_global_phase_aligned_and_zero_stops_new_triggers() {
        let kick_bytes = wav(&[1, 2, 3], false);
        let hat_bytes = wav(&[4, 5, 6], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = 1;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);
        assert_eq!(sequencer.pads()[0].next_frame, Some(22_050));

        beats[1] = 2;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);
        assert_eq!(sequencer.pads()[1].next_frame, Some(11_025));

        beats[2] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 0);
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
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut pattern = Pattern::default();
        assert!(pattern.set_step_enabled(0, 4, false));
        assert!(sequencer.set_pattern(0, pattern));

        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = 4;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);

        let mut output = [0_u32; 1];
        let disabled = sequencer.render(5_513, &mut output);
        assert_eq!(disabled.latest_visual_triggers[0], None);
        assert_eq!(
            disabled.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            0
        );
        assert_eq!(sequencer.pads()[0].tick_ordinal, 2);
        assert_eq!(sequencer.pads()[0].next_frame, Some(11_025));

        let enabled = sequencer.render(11_025, &mut output);
        assert_eq!(enabled.latest_visual_triggers[0], Some(11_025));
        assert_eq!(
            enabled.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            1
        );
    }

    #[test]
    fn changing_base_interval_reschedules_all_enabled_pads() {
        let kick_bytes = wav(&[1; 64], false);
        let hat_bytes = wav(&[4, 5, 6], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = 1;
        beats[6] = 2;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);
        assert_eq!(
            sequencer.queue_preview(PreviewRequest::new(0, DEFAULT_KICK_SAMPLE).unwrap()),
            None
        );
        let mut output = [0_u32; 1];
        let preview = sequencer.render(0, &mut output);
        assert_eq!(preview.preview_voice_start_count, 1);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(1));

        sequencer.apply_timing(&beats, 500, 10_000);
        assert_eq!(sequencer.base_interval_ms(), 500);
        assert_eq!(sequencer.pads()[0].next_frame, Some(11_025));
        assert_eq!(sequencer.pads()[6].next_frame, Some(11_025));
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(1));

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
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
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
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, u64::MAX - 10);
        let deadline = sequencer.pads()[0].next_frame.unwrap();
        assert!(
            deadline < 32,
            "the next deadline should wrap to the new epoch"
        );

        let mut output = [0_u32; 64];
        let report = sequencer.render(u64::MAX - 10, &mut output);
        assert!(report.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()] >= 2);
        assert!(sequencer.pads()[0].next_frame.unwrap() > deadline);
    }

    #[test]
    fn fastest_supported_timing_never_builds_a_frame_backlog() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = MAX_BEAT_MULTIPLIER;
        sequencer.apply_timing(&beats, MIN_BASE_INTERVAL_MS, 0);

        let mut output = [0_u32; AUDIO_BLOCK_FRAMES];
        let report = sequencer.render(0, &mut output);
        let next = sequencer.pads()[0].next_frame.unwrap();
        assert_eq!(sequencer.pads()[0].tick_ordinal, 30);
        assert_eq!(
            report.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            u16::from(sequencer.render_policy().max_starts_per_pad)
        );
        assert_eq!(report.load_shed_trigger_count, 21);
        assert!(!frame_has_reached((AUDIO_BLOCK_FRAMES - 1) as u64, next));
    }

    #[test]
    fn full_policy_spreads_dense_admissions_across_the_block() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = MAX_BEAT_MULTIPLIER;
        sequencer.apply_timing(&beats, MIN_BASE_INTERVAL_MS, 0);

        let mut admitted = [u8::MAX; load_control::FULL_QUALITY_MAX_STARTS_PER_PAD as usize];
        let mut count = 0;
        for offset in 0..AUDIO_BLOCK_FRAMES {
            sequencer.block_frame_offset = offset as u8;
            let mut report = RenderReport::default();
            let _ = sequencer.render_pcm_frame(offset as u64, &mut report);
            if report.scheduled_voice_start_count != 0 {
                admitted[count] = offset as u8;
                count += 1;
            }
        }

        assert_eq!(count, admitted.len());
        assert_eq!(admitted, [5, 18, 35, 48, 65, 82, 100, 112]);
    }

    #[test]
    fn overdue_ticks_coalesce_when_any_due_pattern_entry_is_enabled() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut pattern = Pattern::default();
        pattern.fill(false);
        assert!(pattern.set_step_enabled(2, MAX_BEAT_MULTIPLIER, true));
        assert!(sequencer.set_pattern(0, pattern));

        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = MAX_BEAT_MULTIPLIER;
        sequencer.apply_timing(&beats, MIN_BASE_INTERVAL_MS, 0);

        let mut output = [0_u32; 1];
        let first_frame = sequencer.render(5, &mut output);
        assert_eq!(
            first_frame.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            0
        );
        assert_eq!(sequencer.pads()[0].tick_ordinal, 2);

        // A non-contiguous render catches up ordinals 2 and 3 together. Step
        // 1 is disabled and step 2 is enabled, so they coalesce into one hit.
        let coalesced = sequencer.render(13, &mut output);
        assert_eq!(coalesced.latest_visual_triggers[0], Some(13));
        assert_eq!(
            coalesced.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            1
        );
        assert_eq!(sequencer.pads()[0].tick_ordinal, 4);
    }

    #[test]
    fn same_wav_starts_are_independent_even_on_the_same_frame() {
        let kick_bytes = wav(&[100, 50], false);
        let hat_bytes = wav(&[200, 100], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        beats[1] = DENSE_TEST_DIVISION;
        beats[6] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 0);

        let mut output = [0_u32; 24];
        let report = sequencer.render(0, &mut output);
        assert_eq!(report.latest_visual_triggers[0], Some(23));
        assert_eq!(report.latest_visual_triggers[1], Some(23));
        assert_eq!(report.latest_visual_triggers[6], Some(23));
        assert_eq!(report.scheduled_voice_start_count, 3);
        assert_eq!(
            report.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            2
        );
        assert_eq!(
            report.audible_trigger_counts[DEFAULT_OPEN_HAT_SAMPLE.index()],
            1
        );
        assert_eq!(sequencer.active_voice_count(), 3);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(1));
        assert_eq!(sequencer.active_voice_count_for_pad(1), Some(1));
        assert_eq!(sequencer.active_voice_count_for_pad(6), Some(1));
    }

    #[test]
    fn muting_releases_voices_in_place_but_preserves_phase_and_visuals() {
        let kick_bytes = wav(&[3_200; 128], false);
        let hat_bytes = wav(&[200], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 0);

        let mut one_frame = [0_u32; 1];
        let first = sequencer.render(23, &mut one_frame);
        assert_eq!(first.latest_visual_triggers[0], Some(23));
        assert_eq!(first.scheduled_voice_start_count, 1);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(1));

        sequencer.set_mute_mask((1 << 0) | (1 << 15));
        assert_eq!(sequencer.mute_mask(), 1 << 0);
        // Muting never destroys a voice at the control boundary. It starts a
        // 32-frame release that is owned and rendered by the audio task.
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(1));
        let mut release = [0_u32; DECLICK_FRAMES as usize];
        let muted = sequencer.render(45, &mut release);
        assert_eq!(muted.muted_voice_release_count, 1);
        assert_eq!(muted.latest_visual_triggers[0], Some(67));
        assert_eq!(muted.scheduled_voice_start_count, 0);
        assert_eq!(sequencer.pads()[0].tick_ordinal, 4);
        assert_eq!(sequencer.pads()[0].next_frame, Some(89));
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(0));

        sequencer.set_mute_mask(0);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(0));
        let unmuted = sequencer.render(89, &mut one_frame);
        assert_eq!(unmuted.latest_visual_triggers[0], Some(89));
        assert_eq!(unmuted.scheduled_voice_start_count, 1);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(1));
    }

    #[test]
    fn muted_pad_does_not_prevent_an_independent_same_sample_start() {
        let kick_bytes = wav(&[100, 50], false);
        let hat_bytes = wav(&[200], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        beats[1] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 0);
        sequencer.set_mute_mask(1 << 0);

        let mut output = [0_u32; 1];
        let report = sequencer.render(23, &mut output);
        assert_eq!(report.latest_visual_triggers[0], Some(23));
        assert_eq!(report.latest_visual_triggers[1], Some(23));
        assert_eq!(report.scheduled_voice_start_count, 1);
        assert_eq!(
            report.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            1
        );
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(0));
        assert_eq!(sequencer.active_voice_count_for_pad(1), Some(1));
    }

    #[test]
    fn gain_ramps_take_exactly_64_frames() {
        let mut gain = GainRamp::new(100);
        gain.set_percent(0);
        assert_eq!(gain.next_q16(), 64_512);
        for expected_remaining in (0_u32..63).rev() {
            assert_eq!(gain.next_q16(), expected_remaining * 1_024);
        }
        assert_eq!(gain.next_q16(), 0);

        gain.set_percent(100);
        assert_eq!(gain.next_q16(), 1_024);
        for _ in 1..GAIN_RAMP_FRAMES {
            gain.next_q16();
        }
        assert_eq!(gain.next_q16(), 65_536);
    }

    #[test]
    fn optimized_fixed_point_math_matches_wide_truncating_reference() {
        let samples = [i16::MIN, -32_767, -101, -1, 0, 1, 101, 32_767];
        let gains = [0, 1, 17_891, 32_768, 65_535, 65_536];
        for sample in samples {
            for gain in gains {
                let expected = (i64::from(sample) * i64::from(gain) / i64::from(65_536)) as i32;
                assert_eq!(apply_sample_gain_q16(sample, gain), expected);
            }
        }

        let mix_values = [
            i32::MIN,
            -1_081_344,
            -65_537,
            -1,
            0,
            1,
            65_537,
            1_081_311,
            i32::MAX,
        ];
        for value in mix_values {
            for gain in gains {
                let expected = (i64::from(value) * i64::from(gain) / i64::from(65_536)) as i32;
                assert_eq!(apply_mix_gain_q16(value, gain), expected);
            }
        }

        for value in [i32::from(i16::MIN), -1_001, -1, 0, 1, 1_001, 32_767] {
            for remaining in 0..=DECLICK_FRAMES {
                let expected = value * i32::from(remaining) / i32::from(DECLICK_FRAMES);
                assert_eq!(scale_declick(value, remaining), expected);
            }
        }

        let mut ramp = GainRamp::new(100);
        ramp.set_percent(33);
        let start = 65_536_i32;
        let target = percent_to_q16(33) as i32;
        for elapsed in 1..=GAIN_RAMP_FRAMES {
            let expected =
                start + (target - start) * i32::from(elapsed) / i32::from(GAIN_RAMP_FRAMES);
            assert_eq!(ramp.next_q16(), expected as u32);
        }
    }

    #[test]
    fn captured_trigger_gain_uses_bounded_unit_q16_math() {
        assert_eq!(TriggerGain::from_percent(0), TriggerGain::ZERO);
        assert_eq!(TriggerGain::from_percent(50).q16(), 32_768);
        assert_eq!(TriggerGain::from_percent(100), TriggerGain::FULL);
        assert_eq!(TriggerGain::from_percent(u8::MAX), TriggerGain::FULL);
        for percent in 0..=100_u8 {
            assert_eq!(
                TriggerGain::from_percent(percent).q16(),
                percent_to_q16(percent)
            );
        }

        for live in [0, 1, 655, 32_768, 64_880, 65_535, UNIT_Q16] {
            for captured in [0, 1, 655, 32_768, 64_880, 65_535, UNIT_Q16] {
                let expected = ((u64::from(live) * u64::from(captured)) >> 16) as u32;
                assert_eq!(multiply_unit_q16(live, captured), expected);
            }
        }
    }

    #[test]
    fn overlapping_voices_capture_independent_trigger_gains() {
        let sample_bytes = wav(&[1_000], false);
        let catalog = test_catalog(&sample_bytes, &sample_bytes);
        let allocation = VoiceAllocationState::settled(100, &[100; BEAT_PAD_COUNT]);
        let mut pool = VoicePool::new();
        let mut report = RenderReport::default();
        assert!(pool.start_with_policy_and_trigger_gain(
            0,
            VoiceStart::with_trigger_gain(sample(0), TriggerGain::from_percent(50)),
            StartPriority::Scheduled,
            allocation,
            RenderPolicy::FULL,
            &mut report,
        ));
        assert!(pool.start_with_policy_and_trigger_gain(
            0,
            VoiceStart::full(sample(0)),
            StartPriority::Scheduled,
            allocation,
            RenderPolicy::FULL,
            &mut report,
        ));
        assert_eq!(pool.render(&catalog, &[UNIT_Q16; BEAT_PAD_COUNT]), 1_500);

        // The trigger levels multiply the live pad level; master gain remains
        // the final post-mix stage in Sequencer.
        let mut pool = VoicePool::new();
        assert!(pool.start_with_policy_and_trigger_gain(
            0,
            VoiceStart::with_trigger_gain(sample(0), TriggerGain::from_percent(50)),
            StartPriority::Scheduled,
            allocation,
            RenderPolicy::FULL,
            &mut report,
        ));
        assert!(pool.start_with_policy_and_trigger_gain(
            0,
            VoiceStart::full(sample(0)),
            StartPriority::Scheduled,
            allocation,
            RenderPolicy::FULL,
            &mut report,
        ));
        let mut gains = [UNIT_Q16; BEAT_PAD_COUNT];
        gains[0] = percent_to_q16(50);
        assert_eq!(pool.render(&catalog, &gains), 750);
    }

    #[test]
    fn sequencer_multiplies_trigger_pad_and_master_gain_before_saturation() {
        let sample_bytes = wav(&[10_000], false);
        let mut sequencer = test_sequencer(&sample_bytes, &sample_bytes);
        let mut pad_volumes = [100; BEAT_PAD_COUNT];
        pad_volumes[0] = 50;
        sequencer.set_volumes(50, &pad_volumes);
        for frame in 0..GAIN_RAMP_FRAMES {
            assert_eq!(
                sequencer.render_pcm_frame(u64::from(frame), &mut RenderReport::default()),
                0
            );
        }

        let allocation = VoiceAllocationState::settled(50, &pad_volumes);
        assert!(sequencer.voices.start_with_policy_and_trigger_gain(
            0,
            VoiceStart::with_trigger_gain(sample(0), TriggerGain::from_percent(50)),
            StartPriority::Scheduled,
            allocation,
            RenderPolicy::FULL,
            &mut RenderReport::default(),
        ));
        // 10,000 × 50% trigger × 50% pad × 50% master = 1,250.
        assert_eq!(
            sequencer.render_pcm_frame(64, &mut RenderReport::default()),
            1_250
        );
    }

    #[test]
    fn scheduler_applies_step_gain_and_skips_zero_gain_voice_allocation() {
        let sample_bytes = wav(&[1_000], false);
        let mut sequencer = test_sequencer(&sample_bytes, &sample_bytes);
        let mut volumes = TriggerVolumes::default();
        assert_eq!(volumes.adjust_step(0, -50), Some(50));
        assert_eq!(volumes.adjust_step(1, -100), Some(0));
        assert!(sequencer.set_trigger_volumes(0, &volumes));
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = 2;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);

        let mut report = RenderReport::default();
        assert_eq!(sequencer.render_pcm_frame(11_025, &mut report), 500);
        assert_eq!(report.latest_visual_triggers[0], Some(11_025));
        assert_eq!(report.scheduled_voice_start_count, 1);

        let mut report = RenderReport::default();
        assert_eq!(sequencer.render_pcm_frame(22_050, &mut report), 0);
        assert_eq!(report.latest_visual_triggers[0], Some(22_050));
        assert_eq!(report.scheduled_voice_start_count, 0);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(0));
    }

    #[test]
    fn active_voices_keep_captured_gain_and_previews_ignore_pattern_gain() {
        let sample_bytes = wav(&[1_000, 1_000], false);
        let mut sequencer = test_sequencer(&sample_bytes, &sample_bytes);
        let mut volumes = TriggerVolumes::default();
        assert!(volumes.adjust_all(-50));
        assert!(sequencer.set_trigger_volumes(0, &volumes));
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = 1;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);

        assert_eq!(
            sequencer.render_pcm_frame(22_050, &mut RenderReport::default()),
            500
        );
        assert!(volumes.adjust_all(50));
        assert!(sequencer.set_trigger_volumes(0, &volumes));
        // The already-running sample retains the 50% value captured at start.
        assert_eq!(
            sequencer.render_pcm_frame(22_051, &mut RenderReport::default()),
            500
        );

        let mut preview_only = test_sequencer(&sample_bytes, &sample_bytes);
        let mut zero = TriggerVolumes::default();
        assert!(zero.adjust_all(-100));
        assert!(preview_only.set_trigger_volumes(0, &zero));
        assert_eq!(
            preview_only.queue_preview(PreviewRequest::new(0, DEFAULT_KICK_SAMPLE).unwrap()),
            None
        );
        let mut report = RenderReport::default();
        assert_eq!(preview_only.render_pcm_frame(0, &mut report), 1_000);
        assert_eq!(report.preview_voice_start_count, 1);
    }

    #[test]
    fn recovery_coalescing_uses_the_loudest_due_trigger_gain() {
        let sample_bytes = wav(&[1_000], false);
        let mut sequencer = test_sequencer(&sample_bytes, &sample_bytes);
        let mut pattern = Pattern::default();
        pattern.fill(false);
        assert!(pattern.set_bit(1, true));
        assert!(pattern.set_bit(2, true));
        assert!(sequencer.set_pattern(0, pattern));
        let mut volumes = TriggerVolumes::default();
        assert_eq!(volumes.adjust_step(1, -70), Some(30));
        assert_eq!(volumes.adjust_step(2, -20), Some(80));
        assert!(sequencer.set_trigger_volumes(0, &volumes));
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = MAX_BEAT_MULTIPLIER;
        sequencer.apply_timing(&beats, MIN_BASE_INTERVAL_MS, 0);

        let mut report = RenderReport::default();
        assert_eq!(sequencer.render_pcm_frame(5, &mut report), 0);
        let mut report = RenderReport::default();
        let expected = apply_sample_gain_q16(1_000, TriggerGain::from_percent(80).q16());
        assert_eq!(sequencer.render_pcm_frame(13, &mut report), expected as i16);
        assert_eq!(report.latest_visual_triggers[0], Some(13));
        assert_eq!(report.scheduled_voice_start_count, 1);
    }

    #[test]
    fn voice_pool_caches_active_counts_and_accumulates_its_bounded_range() {
        let sample_bytes = wav(&[i16::MIN], false);
        let catalog = test_catalog(&sample_bytes, &sample_bytes);
        let gains = [65_536; BEAT_PAD_COUNT];
        let volumes = [100; BEAT_PAD_COUNT];
        let allocation = VoiceAllocationState::settled(100, &volumes);
        let mut pool = VoicePool::new();

        // An inactive slot's stale owner is never used to index the gain table.
        pool.primaries[0].owner_pad = u8::MAX;
        assert_eq!(pool.render(&catalog, &gains), 0);

        let mut report = RenderReport::default();
        for serial in 0..PRIMARY_VOICE_COUNT {
            assert!(pool.start(
                serial % BEAT_PAD_COUNT,
                sample(0),
                StartPriority::Scheduled,
                allocation,
                &mut report,
            ));
        }
        assert_eq!(pool.active_voice_count(), PRIMARY_VOICE_COUNT);

        for serial in 0..FADE_TAIL_COUNT {
            let mut voice = PlaybackVoice::idle();
            voice.start(serial % BEAT_PAD_COUNT, sample(0), serial as u64);
            pool.preserve_stolen_voice(voice, FADE_TAIL_COUNT as u8, &mut report);
        }
        assert_eq!(pool.active_tail_count(), FADE_TAIL_COUNT);

        // The most-negative contribution from every bounded stream still fits
        // comfortably in i32, so accumulation needs no per-voice saturation.
        assert_eq!(pool.render(&catalog, &gains), -1_081_344);
        assert_eq!(pool.active_voice_count(), 0);
        assert_eq!(pool.active_tail_count(), 0);
    }

    #[test]
    fn per_pad_gain_is_applied_before_master_and_final_saturation() {
        assert_eq!(scale_audio_percent(101, 50), 50);
        assert_eq!(scale_audio_percent(-101, 50), -50);
        assert_eq!(scale_audio_percent(i32::MAX, 100), i32::MAX);
        assert_eq!(scale_audio_percent(i32::MIN, 100), i32::MIN);
        assert_eq!(scale_audio_percent(123, u8::MAX), 123);

        let kick_bytes = wav(&[20_000; 128], false);
        let hat_bytes = wav(&[-20_000; 128], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut volumes = [100; BEAT_PAD_COUNT];
        volumes[0] = 50;
        sequencer.set_volumes(50, &volumes);
        for frame in 0..GAIN_RAMP_FRAMES {
            assert_eq!(
                sequencer.render_pcm_frame(u64::from(frame), &mut RenderReport::default()),
                0
            );
        }

        let mut report = RenderReport::default();
        let allocation = VoiceAllocationState::settled(50, &volumes);
        assert!(sequencer.voices.start(
            0,
            DEFAULT_KICK_SAMPLE,
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert!(sequencer.voices.start(
            1,
            DEFAULT_KICK_SAMPLE,
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));

        // Pad 0 contributes 10,000 and pad 1 contributes 20,000. The 30,000
        // local sum is mastered to 15,000 before the final i16 saturation.
        assert_eq!(sequencer.render_pcm_frame(64, &mut report), 15_000);
        assert_eq!(report.clipped_frame_count, 0);

        let over_range = [u8::MAX; BEAT_PAD_COUNT];
        sequencer.set_volumes(u8::MAX, &over_range);
        assert_eq!(sequencer.global_volume_percent(), 100);
        assert_eq!(sequencer.pad_volume_percent(0), Some(100));
        assert_eq!(sequencer.pad_volume_percent(BEAT_PAD_COUNT), None);
    }

    #[test]
    fn zero_volume_starts_and_advances_a_voice_silently() {
        let kick_bytes = wav(&[32_000; 128], false);
        let hat_bytes = wav(&[200], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut volumes = [100; BEAT_PAD_COUNT];
        volumes[0] = 0;
        sequencer.set_volumes(100, &volumes);

        // Settle the block-aligned gain change before scheduling the voice.
        for frame in 0..GAIN_RAMP_FRAMES {
            assert_eq!(
                sequencer.render_pcm_frame(u64::from(frame), &mut RenderReport::default()),
                0
            );
        }
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 63);

        let mut report = RenderReport::default();
        assert_eq!(sequencer.render_pcm_frame(67, &mut report), 0);
        assert_eq!(report.latest_visual_triggers[0], Some(67));
        assert_eq!(report.scheduled_voice_start_count, 1);
        assert_eq!(
            report.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()],
            0
        );
        let voice = sequencer
            .voices
            .primaries
            .iter()
            .find(|voice| voice.is_active() && voice.owner_pad() == 0)
            .unwrap();
        assert_eq!(voice.cursor, 1);

        volumes[0] = 100;
        sequencer.set_volumes(100, &volumes);
        assert_eq!(
            sequencer.render_pcm_frame(68, &mut RenderReport::default()),
            500
        );
        let voice = sequencer
            .voices
            .primaries
            .iter()
            .find(|voice| voice.is_active() && voice.owner_pad() == 0)
            .unwrap();
        assert_eq!(voice.cursor, 2);
    }

    #[test]
    fn mute_remains_authoritative_over_nonzero_volume() {
        let kick_bytes = wav(&[100, 50], false);
        let hat_bytes = wav(&[200], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 0);
        sequencer.set_volumes(100, &[100; BEAT_PAD_COUNT]);
        sequencer.set_mute_mask(1);

        let mut report = RenderReport::default();
        assert_eq!(sequencer.render_pcm_frame(23, &mut report), 0);
        assert_eq!(report.latest_visual_triggers[0], Some(23));
        assert_eq!(report.scheduled_voice_start_count, 0);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(0));
    }

    #[test]
    fn sample_ids_clamp_and_pad_defaults_use_the_aku_assets() {
        assert_eq!(SampleId::from_index(0), Some(sample(0)));
        assert_eq!(SampleId::from_index(SAMPLE_COUNT - 1), Some(sample(23)));
        assert_eq!(SampleId::from_index(SAMPLE_COUNT), None);
        assert_eq!(sample(23).clamped_offset(1), sample(23));
        assert_eq!(sample(0).clamped_offset(-1), sample(0));
        assert_eq!(sample(2).clamped_offset(49), sample(23));
        assert_eq!(sample(2).clamped_offset(-51), sample(0));

        assert_eq!(DEFAULT_KICK_SAMPLE, sample(16));
        assert_eq!(DEFAULT_OPEN_HAT_SAMPLE, sample(18));
        assert_eq!(DEFAULT_PAD_SAMPLES[..6], [sample(16); 6]);
        assert_eq!(DEFAULT_PAD_SAMPLES[6..], [sample(18); 3]);
    }

    #[test]
    fn pad_sample_changes_only_affect_future_voice_starts() {
        let first_bytes = wav(&[100; 64], false);
        let second_bytes = wav(&[-200; 64], false);
        let first = WavPcm16::parse(&first_bytes).unwrap();
        let second = WavPcm16::parse(&second_bytes).unwrap();
        let mut samples = [first; SAMPLE_COUNT];
        samples[1] = second;
        let mut sequencer = Sequencer::new(SampleCatalog::new(samples, &TEST_SAMPLE_NAMES));
        assert!(sequencer.set_pad_sample(0, sample(0)));
        assert!(!sequencer.set_pad_sample(BEAT_PAD_COUNT, sample(0)));

        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 0);
        sequencer.block_frame_offset = 23;
        assert_eq!(
            sequencer.render_pcm_frame(23, &mut RenderReport::default()),
            100
        );
        assert!(sequencer.set_pad_sample(0, sample(1)));
        assert_eq!(sequencer.pad_sample(0), Some(sample(1)));

        // The old voice retains sample 0 while the new exact-frame start
        // captures sample 1, so both contribute independently.
        let mut report = RenderReport::default();
        sequencer.block_frame_offset = 45;
        assert_eq!(sequencer.render_pcm_frame(45, &mut report), -100);
        assert_eq!(report.audible_trigger_counts[sample(1).index()], 1);
        assert_eq!(sequencer.active_voice_count_for_pad(0), Some(2));
        assert!(
            sequencer
                .voices
                .primaries
                .iter()
                .any(|voice| voice.is_active() && voice.sample == sample(0))
        );
        assert!(
            sequencer
                .voices
                .primaries
                .iter()
                .any(|voice| voice.is_active() && voice.sample == sample(1))
        );
    }

    #[test]
    fn primary_pool_allows_24_same_pad_voices_then_steals_oldest() {
        let volumes = [100; BEAT_PAD_COUNT];
        let allocation = VoiceAllocationState::settled(100, &volumes);
        let mut pool = VoicePool::new();
        let mut report = RenderReport::default();
        for index in 0..PRIMARY_VOICE_COUNT {
            assert!(pool.start(
                0,
                sample(index % SAMPLE_COUNT),
                StartPriority::Scheduled,
                allocation,
                &mut report,
            ));
        }
        assert_eq!(pool.active_voice_count(), PRIMARY_VOICE_COUNT);
        assert_eq!(pool.active_voice_count_for_pad(0), PRIMARY_VOICE_COUNT);
        assert_eq!(report.same_pad_steal_count, 0);
        assert_eq!(pool.primaries[0].started_serial, 0);

        assert!(pool.start(
            0,
            sample(5),
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert_eq!(report.same_pad_steal_count, 1);
        assert_eq!(pool.primaries[0].sample, sample(5));
        assert_eq!(pool.primaries[0].started_serial, 24);
        assert_eq!(pool.active_tail_count(), 1);
        assert_eq!(pool.tails[0].started_serial, 0);

        // Nine tails are retained. A tenth simultaneous steal replaces the
        // oldest equally-long tail and records the bounded overflow.
        for _ in 1..=FADE_TAIL_COUNT {
            assert!(pool.start(
                0,
                sample(6),
                StartPriority::Scheduled,
                allocation,
                &mut report,
            ));
        }
        assert_eq!(pool.active_tail_count(), FADE_TAIL_COUNT);
        assert_eq!(report.same_pad_steal_count, 10);
        assert_eq!(report.fade_tail_overflow_count, 1);
        assert_eq!(pool.tails[0].started_serial, 9);
    }

    #[test]
    fn render_policy_bounds_primary_and_intermediate_tail_counts() {
        let volumes = [100; BEAT_PAD_COUNT];
        let allocation = VoiceAllocationState::settled(100, &volumes);
        let mut pool = VoicePool::new();
        let mut report = RenderReport::default();
        for _ in 0..PRIMARY_VOICE_COUNT {
            assert!(pool.start(
                0,
                sample(0),
                StartPriority::Scheduled,
                allocation,
                &mut report,
            ));
        }

        let policy = RenderPolicy {
            max_primary_voices: PRIMARY_VOICE_COUNT as u8,
            max_fade_tails: 2,
            ..RenderPolicy::FULL
        };
        for _ in 0..6 {
            assert!(pool.start_with_policy(
                0,
                sample(1),
                StartPriority::Scheduled,
                allocation,
                policy,
                &mut report,
            ));
        }
        assert_eq!(pool.active_tail_count(), 2);
        assert_eq!(report.fade_tail_overflow_count, 4);

        let soft_pressure = RenderPolicy {
            max_primary_voices: 12,
            max_fade_tails: FADE_TAIL_COUNT as u8,
            preserve_stolen_fade_tails: false,
            release_excess_primaries: true,
            trim_excess_primaries: false,
            ..policy
        };
        pool.enforce_policy(soft_pressure, &mut report);
        assert_eq!(pool.active_voice_count(), PRIMARY_VOICE_COUNT);
        assert_eq!(pool.active_tail_count(), 2);
        assert!(!pool.start_with_policy(
            0,
            sample(2),
            StartPriority::Scheduled,
            allocation,
            soft_pressure,
            &mut report,
        ));
        assert_eq!(pool.active_tail_count(), 2);
        assert_eq!(report.load_shed_trigger_count, 1);
        let catalog = test_catalog(KICK_WAV, HAT_WAV);
        for _ in 0..DECLICK_FRAMES {
            let _ = pool.render(&catalog, &[percent_to_q16(100); BEAT_PAD_COUNT]);
        }
        assert_eq!(pool.active_voice_count(), 12);
        assert!(pool.start_with_policy(
            0,
            sample(2),
            StartPriority::Scheduled,
            allocation,
            soft_pressure,
            &mut report,
        ));
        assert_eq!(report.load_shed_fade_tail_count, 1);

        let trimmed = RenderPolicy {
            max_primary_voices: 12,
            max_fade_tails: 0,
            preserve_stolen_fade_tails: false,
            trim_excess_primaries: true,
            ..policy
        };
        pool.enforce_policy(trimmed, &mut report);
        assert_eq!(pool.active_voice_count(), 12);
        assert_eq!(pool.active_tail_count(), 0);
        assert_eq!(report.load_shed_primary_count, 12);
        assert_eq!(report.load_shed_fade_tail_count, 1);
    }

    #[test]
    fn aku_kick_at_28_hz_contracts_cleanly_below_the_measured_wall() {
        let mut sequencer = test_sequencer(KICK_WAV, HAT_WAV);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = 28;
        sequencer.apply_timing(&beats, 1_000, 0);

        let mut output = [0_u32; AUDIO_BLOCK_FRAMES];
        let mut frame = 0_u64;
        let mut peak = 0_usize;
        for _ in 0..300 {
            let report = sequencer.render(frame, &mut output);
            peak = peak.max(usize::from(report.peak_primary_voice_count));
            frame = frame.wrapping_add(AUDIO_BLOCK_FRAMES as u64);
            if sequencer.active_voice_count() == 15 {
                break;
            }
        }
        assert_eq!(peak, 15);
        assert_eq!(sequencer.active_voice_count(), 15);

        sequencer.set_render_policy(RenderPolicy {
            max_primary_voices: 14,
            max_fade_tails: FADE_TAIL_COUNT as u8,
            preserve_stolen_fade_tails: false,
            release_excess_primaries: true,
            trim_excess_primaries: false,
            max_starts_per_pad: 1,
            allow_preview: false,
            dither_quality: DitherQuality::Coarse,
        });
        let report = sequencer.render(frame, &mut output);
        assert_eq!(report.load_shed_primary_count, 1);
        assert!(sequencer.active_voice_count() <= 14);
        frame = frame.wrapping_add(AUDIO_BLOCK_FRAMES as u64);

        let mut saw_visual_tick = report.latest_visual_triggers[0].is_some();
        for _ in 0..8 {
            let report = sequencer.render(frame, &mut output);
            saw_visual_tick |= report.latest_visual_triggers[0].is_some();
            assert!(sequencer.active_voice_count() <= 14);
            frame = frame.wrapping_add(AUDIO_BLOCK_FRAMES as u64);
        }
        assert!(saw_visual_tick);
    }

    #[test]
    fn full_pool_uses_zero_volume_then_global_victims_and_drops_silent_requests() {
        fn full_pool(volumes: &[u8; BEAT_PAD_COUNT]) -> VoicePool {
            let mut pool = VoicePool::new();
            let mut ignored = RenderReport::default();
            let allocation = VoiceAllocationState::settled(100, volumes);
            for index in 0..PRIMARY_VOICE_COUNT {
                assert!(pool.start(
                    index % 8,
                    sample(index % SAMPLE_COUNT),
                    StartPriority::Scheduled,
                    allocation,
                    &mut ignored,
                ));
            }
            pool
        }

        let mut zero_victim_volumes = [100; BEAT_PAD_COUNT];
        zero_victim_volumes[2] = 0;
        let mut pool = full_pool(&zero_victim_volumes);
        let mut report = RenderReport::default();
        let allocation = VoiceAllocationState::settled(100, &zero_victim_volumes);
        assert!(pool.start(
            8,
            sample(23),
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert_eq!(report.zero_volume_steal_count, 1);
        assert_eq!(report.global_steal_count, 0);
        assert_eq!(pool.primaries[2].owner_pad(), 8);
        // Every stolen voice gets a bounded de-click tail, even when its pad's
        // target is zero: the live 64-frame gain ramp may still be audible.
        assert_eq!(pool.active_tail_count(), 1);

        let full_volumes = [100; BEAT_PAD_COUNT];
        let mut pool = full_pool(&full_volumes);
        let mut report = RenderReport::default();
        let allocation = VoiceAllocationState::settled(100, &full_volumes);
        assert!(pool.start(
            8,
            sample(23),
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert_eq!(report.global_steal_count, 1);
        assert_eq!(pool.primaries[0].owner_pad(), 8);
        assert_eq!(pool.active_tail_count(), 1);

        let mut silent_request_volumes = full_volumes;
        silent_request_volumes[8] = 0;
        let mut pool = full_pool(&silent_request_volumes);
        let mut report = RenderReport::default();
        let allocation = VoiceAllocationState::settled(100, &silent_request_volumes);
        assert!(!pool.start(
            8,
            sample(23),
            StartPriority::Scheduled,
            allocation,
            &mut report,
        ));
        assert_eq!(report.silent_trigger_drop_count, 1);
        assert_eq!(report.global_steal_count, 0);
        assert_eq!(pool.active_tail_count(), 0);
    }

    #[test]
    fn silent_requests_wait_for_gain_ramps_before_reusing_full_pool_voices() {
        fn full_pool() -> VoicePool {
            let volumes = [100; BEAT_PAD_COUNT];
            let allocation = VoiceAllocationState::settled(100, &volumes);
            let mut pool = VoicePool::new();
            let mut report = RenderReport::default();
            for index in 0..PRIMARY_VOICE_COUNT {
                assert!(pool.start(
                    index % 8,
                    sample(0),
                    StartPriority::Scheduled,
                    allocation,
                    &mut report,
                ));
            }
            pool
        }

        let full_gain = [65_536; BEAT_PAD_COUNT];
        let mut pad_targets = [100; BEAT_PAD_COUNT];
        pad_targets[0] = 0;
        let ramping_pad = VoiceAllocationState::new(100, &pad_targets, 65_536, &full_gain);
        let mut pool = full_pool();
        let mut report = RenderReport::default();
        assert!(!pool.start(
            0,
            sample(1),
            StartPriority::Scheduled,
            ramping_pad,
            &mut report,
        ));
        assert_eq!(report.silent_trigger_drop_count, 1);
        assert_eq!(report.same_pad_steal_count, 0);
        assert_eq!(pool.active_tail_count(), 0);

        // A zero master makes every new request silent, but its in-progress
        // ramp still protects voices that are currently audible.
        let ramping_master =
            VoiceAllocationState::new(0, &[100; BEAT_PAD_COUNT], 65_536, &full_gain);
        let mut pool = full_pool();
        let mut report = RenderReport::default();
        assert!(!pool.start(
            8,
            sample(1),
            StartPriority::Scheduled,
            ramping_master,
            &mut report,
        ));
        assert_eq!(report.silent_trigger_drop_count, 1);
        assert_eq!(report.zero_volume_steal_count, 0);

        // Once the master reaches zero, the same request may reuse a truly
        // silent target-zero victim without sacrificing audible output.
        let settled_master = VoiceAllocationState::new(0, &[100; BEAT_PAD_COUNT], 0, &full_gain);
        assert!(pool.start(
            8,
            sample(1),
            StartPriority::Scheduled,
            settled_master,
            &mut report,
        ));
        assert_eq!(report.zero_volume_steal_count, 1);
        assert_eq!(pool.active_tail_count(), 1);
    }

    #[test]
    fn audible_request_fades_a_target_zero_victim_still_ramping_down() {
        let full_volumes = [100; BEAT_PAD_COUNT];
        let fill = VoiceAllocationState::settled(100, &full_volumes);
        let mut pool = VoicePool::new();
        let mut report = RenderReport::default();
        for index in 0..PRIMARY_VOICE_COUNT {
            assert!(pool.start(
                index % 8,
                sample(0),
                StartPriority::Scheduled,
                fill,
                &mut report,
            ));
        }

        let mut targets = full_volumes;
        targets[2] = 0;
        let ramping = VoiceAllocationState::new(100, &targets, 65_536, &[65_536; BEAT_PAD_COUNT]);
        let mut report = RenderReport::default();
        assert!(pool.start(8, sample(1), StartPriority::Scheduled, ramping, &mut report,));
        assert_eq!(report.zero_volume_steal_count, 1);
        assert_eq!(pool.active_tail_count(), 1);
        assert_eq!(pool.tails[0].owner_pad, 2);
    }

    #[test]
    fn previews_only_use_free_or_same_pad_primary_slots() {
        let volumes = [100; BEAT_PAD_COUNT];
        let allocation = VoiceAllocationState::settled(100, &volumes);
        let mut pool = VoicePool::new();
        let mut report = RenderReport::default();
        for index in 0..PRIMARY_VOICE_COUNT {
            assert!(pool.start(
                index % 8,
                sample(0),
                StartPriority::Scheduled,
                allocation,
                &mut report,
            ));
        }

        assert!(!pool.start(
            8,
            sample(1),
            StartPriority::Preview,
            allocation,
            &mut report,
        ));
        assert_eq!(report.preview_drop_count, 1);
        assert_eq!(report.global_steal_count, 0);
        assert_eq!(report.zero_volume_steal_count, 0);

        assert!(pool.start(
            3,
            sample(1),
            StartPriority::Preview,
            allocation,
            &mut report,
        ));
        assert_eq!(report.same_pad_steal_count, 1);
        assert_eq!(pool.primaries[3].sample, sample(1));
    }

    #[test]
    fn forced_voice_and_steal_tail_fades_last_exactly_32_frames() {
        let sample_bytes = wav(&[32_000; 64], false);
        let catalog = test_catalog(&sample_bytes, &sample_bytes);

        let mut voice = PlaybackVoice::idle();
        voice.start(0, DEFAULT_KICK_SAMPLE, 7);
        assert!(voice.force_release());
        for remaining in (1_u8..=DECLICK_FRAMES).rev() {
            assert_eq!(voice.render(&catalog, 65_536), i32::from(remaining) * 1_000);
        }
        assert!(!voice.is_active());

        let mut stolen = PlaybackVoice::idle();
        stolen.start_with_trigger_gain(0, DEFAULT_KICK_SAMPLE, TriggerGain::from_percent(50), 8);
        let mut tail = FadeTail::idle();
        tail.start_from(stolen);
        assert_eq!(tail.trigger_gain_q16, percent_to_q16(50));
        for remaining in (1_u8..=DECLICK_FRAMES).rev() {
            assert_eq!(tail.render(&catalog, 65_536), i32::from(remaining) * 500);
        }
        assert!(!tail.active);
    }

    #[test]
    fn fade_tail_overflow_replaces_nearest_completion_then_oldest() {
        let mut pool = VoicePool::new();
        pool.next_serial = 100;
        for (slot, tail) in pool.tails.iter_mut().enumerate() {
            let mut voice = PlaybackVoice::idle();
            voice.start(slot % BEAT_PAD_COUNT, sample(0), 50 + slot as u64);
            tail.start_from(voice);
            tail.remaining = 20;
        }
        pool.tails[2].remaining = 5;
        pool.tails[2].started_serial = 90;
        pool.tails[6].remaining = 5;
        pool.tails[6].started_serial = 80;

        let mut incoming = PlaybackVoice::idle();
        incoming.start(8, sample(1), 101);
        let mut report = RenderReport::default();
        pool.preserve_stolen_voice(incoming, FADE_TAIL_COUNT as u8, &mut report);
        assert_eq!(report.fade_tail_overflow_count, 1);
        assert_eq!(pool.tails[6].started_serial, 101);
        assert_eq!(pool.tails[2].started_serial, 90);
    }

    #[test]
    fn muting_one_pad_releases_all_24_of_its_primary_voices_in_place() {
        let volumes = [100; BEAT_PAD_COUNT];
        let allocation = VoiceAllocationState::settled(100, &volumes);
        let mut pool = VoicePool::new();
        let mut report = RenderReport::default();
        for _ in 0..PRIMARY_VOICE_COUNT {
            assert!(pool.start(
                0,
                sample(0),
                StartPriority::Scheduled,
                allocation,
                &mut report,
            ));
        }
        assert_eq!(pool.release_mask(1), PRIMARY_VOICE_COUNT as u16);
        assert_eq!(pool.release_mask(1), 0);
        assert_eq!(pool.active_voice_count(), PRIMARY_VOICE_COUNT);
        assert_eq!(pool.active_tail_count(), 0);
    }

    #[test]
    fn scheduled_and_latest_preview_start_independently_and_muted_preview_pulses() {
        let sample_bytes = wav(&[100; 64], false);
        let mut sequencer = test_sequencer(&sample_bytes, &sample_bytes);
        let volumes = [100; BEAT_PAD_COUNT];
        let allocation = VoiceAllocationState::settled(100, &volumes);
        let mut ignored = RenderReport::default();
        for index in 0..PRIMARY_VOICE_COUNT - 1 {
            assert!(sequencer.voices.start(
                1 + index % 7,
                sample(0),
                StartPriority::Scheduled,
                allocation,
                &mut ignored,
            ));
        }
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = DENSE_TEST_DIVISION;
        sequencer.apply_timing(&beats, DENSE_TEST_INTERVAL_MS, 0);
        let first = PreviewRequest::new(8, sample(1)).unwrap();
        let second = PreviewRequest::new(7, sample(2)).unwrap();
        assert_eq!(sequencer.queue_preview(first), None);
        assert_eq!(sequencer.queue_preview(second), Some(first));

        let mut report = RenderReport::default();
        assert_eq!(sequencer.render_pcm_frame(23, &mut report), 2_500);
        assert_eq!(report.scheduled_voice_start_count, 1);
        assert_eq!(report.preview_voice_start_count, 1);
        assert_eq!(report.preview_drop_count, 0);
        assert_eq!(report.same_pad_steal_count, 1);
        assert_eq!(report.global_steal_count, 0);
        assert_eq!(report.latest_visual_triggers[0], Some(23));
        assert_eq!(report.latest_visual_triggers[7], Some(23));
        assert_eq!(report.peak_primary_voice_count, PRIMARY_VOICE_COUNT as u8);

        let mut muted = test_sequencer(&sample_bytes, &sample_bytes);
        muted.set_mute_mask(1 << 2);
        assert_eq!(
            muted.queue_preview(PreviewRequest::new(2, sample(3)).unwrap()),
            None
        );
        let mut report = RenderReport::default();
        assert_eq!(muted.render_pcm_frame(99, &mut report), 0);
        assert_eq!(report.latest_visual_triggers[2], Some(99));
        assert_eq!(report.preview_drop_count, 1);
        assert_eq!(report.preview_voice_start_count, 0);
    }

    #[test]
    fn shared_sample_selection_is_checked_and_preview_queue_is_latest_wins() {
        let mut state = SharedState::default();
        assert_eq!(state.pad_samples(), &DEFAULT_PAD_SAMPLES);
        assert!(state.set_pad_sample(4, sample(23)));
        assert_eq!(state.pad_sample(4), Some(sample(23)));
        assert!(!state.set_pad_sample(BEAT_PAD_COUNT, sample(0)));
        assert_eq!(state.pad_sample(BEAT_PAD_COUNT), None);

        let first = PreviewRequest::new(4, sample(22)).unwrap();
        let second = PreviewRequest::new(4, sample(23)).unwrap();
        assert_eq!(PreviewRequest::new(BEAT_PAD_COUNT, sample(0)), None);
        assert_eq!(state.queue_preview(first), None);
        assert_eq!(state.queue_preview(second), Some(first));
        assert_eq!(state.take_preview(), Some(second));
        assert_eq!(state.take_preview(), None);
    }

    #[test]
    fn sampler_diagnostics_accumulate_counts_and_peak_usage() {
        let first = RenderReport {
            scheduled_voice_start_count: 2,
            preview_voice_start_count: 1,
            same_pad_steal_count: 3,
            zero_volume_steal_count: 4,
            global_steal_count: 5,
            silent_trigger_drop_count: 6,
            preview_drop_count: 7,
            fade_tail_overflow_count: 8,
            muted_voice_release_count: 9,
            clipped_frame_count: 10,
            peak_primary_voice_count: 20,
            peak_fade_tail_count: 5,
            peak_total_voice_count: 25,
            ..RenderReport::default()
        };
        let second = RenderReport {
            scheduled_voice_start_count: 11,
            preview_voice_start_count: 12,
            peak_primary_voice_count: 24,
            peak_fade_tail_count: 3,
            peak_total_voice_count: 27,
            ..RenderReport::default()
        };
        let mut state = SharedState::default();
        state.record_sampler_report(&first);
        state.record_sampler_report(&second);
        assert_eq!(state.sampler_diagnostics.scheduled_voice_start_count, 13);
        assert_eq!(state.sampler_diagnostics.preview_voice_start_count, 13);
        assert_eq!(state.sampler_diagnostics.same_pad_steal_count, 3);
        assert_eq!(state.sampler_diagnostics.zero_volume_steal_count, 4);
        assert_eq!(state.sampler_diagnostics.global_steal_count, 5);
        assert_eq!(state.sampler_diagnostics.silent_trigger_drop_count, 6);
        assert_eq!(state.sampler_diagnostics.preview_drop_count, 7);
        assert_eq!(state.sampler_diagnostics.fade_tail_overflow_count, 8);
        assert_eq!(state.sampler_diagnostics.muted_voice_release_count, 9);
        assert_eq!(state.sampler_diagnostics.clipped_frame_count, 10);
        assert_eq!(state.sampler_diagnostics.peak_primary_voice_count, 24);
        assert_eq!(state.sampler_diagnostics.peak_fade_tail_count, 5);
        assert_eq!(state.sampler_diagnostics.peak_total_voice_count, 27);
    }

    #[test]
    fn long_run_trigger_counts_do_not_drift() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = test_sequencer(&kick_bytes, &hat_bytes);
        let mut beats = [0; BEAT_PAD_COUNT];
        beats[0] = 3;
        sequencer.apply_timing(&beats, DEFAULT_BASE_INTERVAL_MS, 0);

        let mut total = 0_u32;
        let mut buffer = [0_u32; AUDIO_BLOCK_FRAMES];
        let end = SAMPLE_RATE as u64 * 10;
        let mut frame = 0_u64;
        while frame < end {
            let count = ((end - frame) as usize).min(AUDIO_BLOCK_FRAMES);
            let report = sequencer.render(frame, &mut buffer[..count]);
            total += u32::from(report.audible_trigger_counts[DEFAULT_KICK_SAMPLE.index()]);
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
    fn keys_debounce_press_and_release_edges() {
        let mut debounce = KeyDebouncer::new(3);
        assert_eq!(debounce.update(1), KeyChanges::default());
        assert_eq!(debounce.update(1), KeyChanges::default());
        let press_zero = debounce.update(1);
        assert_eq!(press_zero.pressed, 1);
        assert_eq!(debounce.update(0), KeyChanges::default());
        assert_eq!(debounce.update(0), KeyChanges::default());
        assert_eq!(debounce.update(0).released, 1);
    }

    #[test]
    fn physical_control_keys_debounce_but_never_become_selected_beats() {
        assert_eq!(KEY_COUNT, 12);
        assert_eq!(BEAT_PAD_COUNT, 9);
        assert_eq!(MUTE_KEY_INDEX, 9);
        assert_eq!(VOLUME_KEY_INDEX, 10);
        assert_eq!(RETURN_KEY_INDEX, 11);
        assert_eq!(BEAT_PAD_MASK, 0x01ff);

        let controls = (1 << MUTE_KEY_INDEX) | (1 << VOLUME_KEY_INDEX) | (1 << RETURN_KEY_INDEX);
        let mut debounce = KeyDebouncer::new(2);
        assert_eq!(debounce.update(controls), KeyChanges::default());
        let changes = debounce.update(controls);
        assert_eq!(changes.pressed, controls);
        assert_eq!(debounce.stable_mask(), controls);

        let mut selection = VoiceSelection::new();
        for pad in 0..BEAT_PAD_COUNT {
            if changes.pressed & (1 << pad) != 0 {
                selection.toggle_exclusive(pad);
            }
        }
        assert_eq!(selection.selected(), None);
        assert_eq!(
            MuteTarget::for_selected_pad(selection.selected()),
            MuteTarget::Global
        );

        // Controls applies the beat-key changes before capturing the mute
        // target, so a beat and Mute reaching debounce together target the beat.
        selection.toggle_exclusive(4);
        assert_eq!(selection.selected(), Some(4));
        assert_eq!(
            MuteTarget::for_selected_pad(selection.selected()),
            MuteTarget::Pad(4)
        );
    }

    #[test]
    fn mute_button_captures_target_and_uses_exclusive_tap_threshold() {
        let mut button = MuteButtonState::new();
        assert!(button.press(MuteTarget::Pad(2), 100));
        assert_eq!(button.active_target(), Some(MuteTarget::Pad(2)));
        assert!(!button.press(MuteTarget::Global, 110));
        assert_eq!(button.active_target(), Some(MuteTarget::Pad(2)));
        assert_eq!(
            button.release(399),
            Some(MuteRelease {
                target: MuteTarget::Pad(2),
                tapped: true,
            })
        );
        assert_eq!(button.active_target(), None);
        assert_eq!(button.release(400), None);

        assert!(button.press(MuteTarget::Global, 1_000));
        assert_eq!(
            button.release(1_300),
            Some(MuteRelease {
                target: MuteTarget::Global,
                tapped: false,
            })
        );
        assert!(!button.press(MuteTarget::Pad(BEAT_PAD_COUNT), 2_000));
    }

    #[test]
    fn shared_mutes_combine_global_pad_latched_and_momentary_state() {
        let mut state = SharedState::default();
        assert_eq!(state.effective_mute_mask(), 0);
        assert_eq!(state.latched_mute(MuteTarget::Global), Some(false));
        assert_eq!(state.latched_mute(MuteTarget::Pad(3)), Some(false));
        assert_eq!(state.latched_mute(MuteTarget::Pad(BEAT_PAD_COUNT)), None);

        assert!(state.begin_mute_gesture(MuteTarget::Pad(3)));
        assert!(!state.begin_mute_gesture(MuteTarget::Global));
        assert_eq!(state.effective_mute_mask(), 1 << 3);
        assert_eq!(state.mute_indicator_active(MuteTarget::Pad(3)), Some(true));
        assert_eq!(state.mute_indicator_active(MuteTarget::Global), Some(false));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(3),
            tapped: true,
        }));
        assert_eq!(state.latched_mute(MuteTarget::Pad(3)), Some(true));
        assert_eq!(state.effective_mute_mask(), 1 << 3);

        // A hold over an already latched mute is momentary-only and must not
        // clear that persistent setting on release.
        assert!(state.begin_mute_gesture(MuteTarget::Pad(3)));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(3),
            tapped: false,
        }));
        assert_eq!(state.latched_mute(MuteTarget::Pad(3)), Some(true));
        assert_eq!(state.effective_mute_mask(), 1 << 3);

        assert!(state.begin_mute_gesture(MuteTarget::Global));
        assert_eq!(state.effective_mute_mask(), BEAT_PAD_MASK);
        // A selected pad indicator deliberately ignores global state.
        assert_eq!(state.mute_indicator_active(MuteTarget::Pad(2)), Some(false));
        assert_eq!(state.mute_indicator_active(MuteTarget::Global), Some(true));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Global,
            tapped: false,
        }));
        assert_eq!(state.effective_mute_mask(), 1 << 3);

        assert!(state.begin_mute_gesture(MuteTarget::Global));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Global,
            tapped: true,
        }));
        assert_eq!(state.latched_mute(MuteTarget::Global), Some(true));
        assert_eq!(state.effective_mute_mask(), BEAT_PAD_MASK);

        assert!(state.begin_mute_gesture(MuteTarget::Pad(3)));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(3),
            tapped: true,
        }));
        assert_eq!(state.latched_mute(MuteTarget::Pad(3)), Some(false));
        assert_eq!(state.effective_mute_mask(), BEAT_PAD_MASK);

        assert!(state.begin_mute_gesture(MuteTarget::Global));
        assert!(!state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(0),
            tapped: true,
        }));
        assert_eq!(state.effective_mute_mask(), BEAT_PAD_MASK);
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Global,
            tapped: true,
        }));
        assert_eq!(state.effective_mute_mask(), 0);
    }

    #[test]
    fn volume_targets_are_live_and_shared_values_default_and_clamp() {
        assert_eq!(VolumeTarget::for_selected_pad(None), VolumeTarget::Global);
        assert_eq!(
            VolumeTarget::for_selected_pad(Some(4)),
            VolumeTarget::Pad(4)
        );
        assert_eq!(
            VolumeTarget::for_selected_pad(Some(BEAT_PAD_COUNT)),
            VolumeTarget::Global
        );

        let mut state = SharedState::default();
        assert_eq!(state.global_volume_percent(), DEFAULT_VOLUME_PERCENT);
        assert_eq!(
            state.pad_volume_percents(),
            &[DEFAULT_VOLUME_PERCENT; BEAT_PAD_COUNT]
        );
        assert_eq!(state.volume_percent(VolumeTarget::Global), Some(100));
        assert_eq!(state.volume_percent(VolumeTarget::Pad(3)), Some(100));
        assert_eq!(
            state.volume_percent(VolumeTarget::Pad(BEAT_PAD_COUNT)),
            None
        );

        assert_eq!(state.adjust_volume(VolumeTarget::Global, -1), Some(99));
        assert_eq!(state.adjust_volume(VolumeTarget::Pad(3), -10), Some(90));
        assert_eq!(state.adjust_volume(VolumeTarget::Pad(3), -1_000), Some(0));
        assert_eq!(state.adjust_volume(VolumeTarget::Pad(3), 1_000), Some(100));
        assert_eq!(
            state.adjust_volume(VolumeTarget::Pad(BEAT_PAD_COUNT), -10),
            None
        );
        assert_eq!(state.global_volume_percent(), 99);
    }

    #[test]
    fn pattern_cursor_clamps_and_display_window_tracks_it() {
        assert_eq!(adjust_pattern_cursor(0, 8, -1), 0);
        assert_eq!(adjust_pattern_cursor(8, 8, 1), 8);
        assert_eq!(adjust_pattern_cursor(1, 8, 10), 8);
        assert_eq!(adjust_pattern_cursor(7, 8, -10), 0);
        assert_eq!(adjust_pattern_cursor(123, 0, 10), 0);
        assert_eq!(adjust_pattern_cursor(0, MAX_BEAT_MULTIPLIER, -1), 0);
        assert_eq!(
            adjust_pattern_cursor(MAX_BEAT_MULTIPLIER, MAX_BEAT_MULTIPLIER, 1),
            MAX_BEAT_MULTIPLIER
        );

        assert_eq!(pattern_window_start(0, 12, 5), 0);
        assert_eq!(pattern_window_start(4, 12, 5), 2);
        assert_eq!(pattern_window_start(12, 12, 5), 8);
        assert_eq!(pattern_window_start(3, 4, 5), 0);
        assert_eq!(pattern_window_start(0, 0, 5), 0);
        assert_eq!(pattern_window_start(3, 12, 0), 0);

        assert_eq!(
            scroll_menu_window(0, 7, 5),
            ScrollMenuWindow {
                start: 0,
                item_rows: 5,
                more_above: false,
                more_below: true,
            }
        );
        assert_eq!(
            scroll_menu_window(4, 20, 5),
            ScrollMenuWindow {
                start: 1,
                item_rows: 5,
                more_above: true,
                more_below: true,
            }
        );
        assert_eq!(scroll_menu_window(5, 20, 5).start, 2);
        assert_eq!(scroll_menu_window(6, 20, 5).start, 3);
        assert_eq!(
            scroll_menu_window(19, 20, 5),
            ScrollMenuWindow {
                start: 15,
                item_rows: 5,
                more_above: true,
                more_below: false,
            }
        );
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
        assert_eq!(accelerated_encoder_delta(1, Some(41)), 1);
        assert_eq!(accelerated_encoder_delta(1, Some(40)), 10);
        assert_eq!(accelerated_encoder_delta(-1, Some(20)), -10);
        assert_eq!(adjust_volume_percent(100, -1), 99);
        assert_eq!(adjust_volume_percent(5, -10), 0);
        assert_eq!(adjust_volume_percent(95, 10), 100);
        assert_eq!(sample(0).clamped_offset(-1), sample(0));
        assert_eq!(
            sample(SAMPLE_COUNT - 1).clamped_offset(1),
            sample(SAMPLE_COUNT - 1)
        );
        assert_eq!(adjust_sample_selection(sample(0), -10), sample(0));
        assert_eq!(adjust_sample_selection(sample(23), 10), sample(23));
        assert_eq!(adjust_sample_selection(sample(7), 0), sample(7));

        let mut acceleration = UiEncoderAcceleration::new();
        assert_eq!(
            acceleration.update(1_000, UiEncoderTarget::BeatsPad(3), 1),
            1
        );
        assert_eq!(
            acceleration.update(1_030, UiEncoderTarget::BeatsPad(3), 1),
            10
        );
        assert_eq!(acceleration.update(1_060, UiEncoderTarget::Light, 1), 1);
        assert_eq!(
            acceleration.update(1_065, UiEncoderTarget::Volume(VolumeTarget::Pad(3)), 1,),
            1
        );
        assert_eq!(
            acceleration.update(1_070, UiEncoderTarget::BeatsGlobal, 1),
            1
        );
        assert_eq!(
            acceleration.update(1_080, UiEncoderTarget::BeatsGlobal, -1),
            -1
        );
        assert_eq!(
            acceleration.update(1_090, UiEncoderTarget::BeatsGlobal, -1),
            -10
        );
        let trigger = UiEncoderTarget::PatternVolume(PatternVolumeTarget::Step { pad: 3, step: 7 });
        assert_eq!(acceleration.update(1_200, trigger, 1), 1);
        assert_eq!(acceleration.update(1_230, trigger, 1), 10);
        assert_eq!(
            acceleration.update(
                1_235,
                UiEncoderTarget::PatternVolume(PatternVolumeTarget::Step { pad: 3, step: 8 }),
                1,
            ),
            1
        );

        assert_eq!(adjust_led_brightness(50, -10), 40);
        assert_eq!(adjust_led_brightness(40, -1_000), 0);
        assert_eq!(adjust_led_brightness(0, 1_000), 100);

        assert_eq!(scale_color((200, 100, 50), 100), (200, 100, 50));
        assert_eq!(scale_color((200, 100, 50), 50), (100, 50, 25));
        assert_eq!(scale_color((200, 100, 50), 0), (0, 0, 0));
        assert_eq!(mute_led_color(false, 100), (255, 0, 0));
        assert_eq!(mute_led_color(true, 100), (51, 0, 0));
        assert_eq!(mute_led_color(false, 50), (128, 0, 0));
        assert_eq!(mute_led_color(true, 50), (26, 0, 0));
        assert_eq!(mute_led_color(false, 0), (0, 0, 0));
        assert_eq!(volume_led_color(100, 100), (255, 255, 0));
        assert_eq!(volume_led_color(50, 100), (128, 128, 0));
        assert_eq!(volume_led_color(100, 50), (128, 128, 0));
        assert_eq!(volume_led_color(50, 50), (64, 64, 0));
        assert_eq!(volume_led_color(0, 100), (0, 0, 0));
        assert_eq!(volume_led_color(100, 0), (0, 0, 0));
        assert_eq!(return_led_color(0), (0, 0, 0));
        assert_eq!(return_led_color(50), (128, 128, 128));
        assert_eq!(return_led_color(100), (255, 255, 255));
        assert_eq!(dim_nonselected_led_color((0, 0, 0), true, false), (0, 0, 0));
        assert_eq!(
            dim_nonselected_led_color((128, 64, 1), true, false),
            (26, 13, 0)
        );
        assert_eq!(
            dim_nonselected_led_color((128, 64, 1), false, false),
            (128, 64, 1)
        );
        assert_eq!(
            dim_nonselected_led_color((128, 64, 1), true, true),
            (128, 64, 1)
        );
        assert_eq!(
            voice_led_color(0, 50, false, false, false, false),
            (0, 0, 0)
        );
        assert_eq!(voice_led_color(0, 50, true, false, false, false), (0, 0, 0));
        assert_eq!(
            voice_led_color(0, 50, true, true, false, false),
            (128, 128, 128)
        );
        assert_eq!(
            voice_led_color(0, 50, false, false, true, false),
            (128, 0, 0)
        );
        assert_eq!(voice_led_color(0, 50, true, false, true, false), (26, 0, 0));
        assert_eq!(voice_led_color(0, 50, true, false, false, true), (26, 0, 0));
        assert_eq!(voice_led_color(0, 50, true, true, true, false), (128, 0, 0));
        assert_eq!(
            voice_led_color(0, 50, true, true, false, true),
            (128, 128, 128)
        );
        assert_eq!(
            voice_led_color(0, 50, false, false, false, true),
            (128, 0, 0)
        );
        assert_eq!(voice_led_color(0, 0, true, true, true, true), (0, 0, 0));
        assert_eq!(colorwheel(0), (255, 0, 0));
        assert_eq!(colorwheel(85), (0, 255, 0));
        assert_eq!(colorwheel(170), (0, 0, 255));
        assert!(!led_pulse_active(0, 0, 2_205));
        assert!(led_pulse_active(1_100, 1_000, 2_205));
        assert!(!led_pulse_active(3_205, 1_000, 2_205));
        assert!(led_pulse_active(5, u64::MAX - 4, 20));
    }

    #[test]
    fn persistent_voice_selection_is_exclusive_but_mask_storage_is_multi_capable() {
        let mut selection = VoiceSelection::from_mask((1 << 1) | (1 << 7) | (1 << 14));
        assert_eq!(selection.mask(), (1 << 1) | (1 << 7));
        assert_eq!(selection.count(), 2);
        assert_eq!(selection.selected(), None);
        assert!(selection.contains(1));
        assert!(selection.insert(4));
        assert_eq!(selection.count(), 3);
        assert!(selection.remove(7));
        assert!(selection.toggle(8));
        assert!(!selection.toggle(BEAT_PAD_COUNT));

        assert!(selection.toggle_exclusive(3));
        assert_eq!(selection.selected(), Some(3));
        assert!(selection.toggle_exclusive(3));
        assert_eq!(selection.mask(), 0);
        assert!(selection.select_exclusive(8));
        assert_eq!(selection.selected(), Some(8));
        selection.clear();
        assert_eq!(selection, VoiceSelection::new());
    }

    #[test]
    fn sample_browsing_preview_is_suppressed_while_the_encoder_button_is_held() {
        let selected = adjust_sample_selection(sample(4), 1);
        assert_eq!(selected, sample(5));
        assert_eq!(
            sample_selection_preview_request(3, sample(4), selected, false),
            PreviewRequest::new(3, sample(5))
        );
        assert_eq!(
            sample_selection_preview_request(3, sample(4), selected, true),
            None
        );
        let first = adjust_sample_selection(sample(0), -1);
        let last = adjust_sample_selection(sample(SAMPLE_COUNT - 1), 1);
        assert_eq!(first, sample(0));
        assert_eq!(last, sample(SAMPLE_COUNT - 1));
        assert_eq!(
            sample_selection_preview_request(3, sample(0), first, false),
            None
        );
        assert_eq!(
            sample_selection_preview_request(3, sample(SAMPLE_COUNT - 1), last, false),
            None
        );
        assert_eq!(
            sample_selection_preview_request(BEAT_PAD_COUNT, sample(4), selected, false,),
            None
        );
    }

    #[test]
    fn ui_controller_keeps_modes_open_clamps_root_and_remembers_pattern_cursors() {
        assert_eq!(
            RootMode::ALL,
            [
                RootMode::Beats,
                RootMode::Pattern,
                RootMode::Sample,
                RootMode::Light,
                RootMode::Save,
                RootMode::Songs,
                RootMode::ResetAll,
            ]
        );
        assert_eq!(RootMode::default(), RootMode::Beats);
        let mut ui = UiController::new();
        assert_eq!(ui.page(), UiPage::Root);
        assert_eq!(ui.root_mode(), RootMode::Beats);
        ui.rotate_root(-1);
        assert_eq!(ui.root_mode(), RootMode::Beats);
        ui.rotate_root(100);
        assert_eq!(ui.root_mode(), RootMode::ResetAll);
        ui.rotate_root(1);
        assert_eq!(ui.root_mode(), RootMode::ResetAll);
        ui.rotate_root(-100);
        assert_eq!(ui.root_mode(), RootMode::Beats);
        ui.rotate_root(1);
        assert_eq!(ui.root_mode(), RootMode::Pattern);
        ui.enter_root_mode();
        assert_eq!(ui.page(), UiPage::Pattern);
        assert_eq!(ui.encoder_target(false), UiEncoderTarget::PatternNone);

        assert!(ui.press_voice(2));
        ui.rotate_pattern(2, 8, 3);
        assert_eq!(ui.pattern_cursor(2), Some(3));
        assert!(ui.press_voice(6));
        assert_eq!(ui.page(), UiPage::Pattern);
        ui.rotate_pattern(6, 4, 2);
        assert_eq!(ui.pattern_cursor(6), Some(2));
        assert!(ui.press_voice(2));
        assert_eq!(ui.pattern_cursor(2), Some(3));
        let mut loaded_divisions = [MAX_BEAT_MULTIPLIER; BEAT_PAD_COUNT];
        loaded_divisions[2] = 1;
        loaded_divisions[6] = 0;
        ui.clamp_pattern_cursors(&loaded_divisions);
        assert_eq!(ui.pattern_cursor(2), Some(1));
        assert_eq!(ui.pattern_cursor(6), Some(0));

        ui.return_to_root(1 << RETURN_KEY_INDEX, false);
        assert_eq!(ui.page(), UiPage::Root);
        assert_eq!(ui.root_mode(), RootMode::Pattern);
        assert_eq!(ui.selected_pad(), Some(2));
        assert_eq!(ui.pattern_cursor(2), Some(1));
        assert!(ui.key_suppressed(RETURN_KEY_INDEX));
        ui.update_suppression(1 << RETURN_KEY_INDEX, false);
        assert!(ui.key_suppressed(RETURN_KEY_INDEX));
        ui.update_suppression(0, false);
        assert!(!ui.key_suppressed(RETURN_KEY_INDEX));
        ui.return_to_root(0, false);
        assert_eq!(ui.selected_pad(), None);
    }

    #[test]
    fn ui_controller_routes_context_targets_and_confirmation_actions_safely() {
        let mut ui = UiController::new();
        assert_eq!(ui.root_mode(), RootMode::Beats);
        ui.enter_root_mode();
        assert_eq!(ui.encoder_target(false), UiEncoderTarget::BeatsGlobal);
        ui.press_voice(4);
        assert_eq!(ui.encoder_target(false), UiEncoderTarget::BeatsPad(4));
        assert_eq!(
            ui.encoder_target(true),
            UiEncoderTarget::Volume(VolumeTarget::Pad(4))
        );
        ui.press_voice(4);
        assert_eq!(
            ui.encoder_target(true),
            UiEncoderTarget::Volume(VolumeTarget::Global)
        );

        ui.return_to_root(0, false);
        ui.rotate_root(100);
        assert_eq!(ui.root_mode(), RootMode::ResetAll);
        ui.press_voice(1);
        ui.enter_root_mode();
        assert_eq!(ui.page(), UiPage::ResetAll);
        assert_eq!(ui.reset_choice(), ResetAllChoice::Cancel);
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(ui.page(), UiPage::Root);
        assert_eq!(ui.selected_pad(), Some(1));

        ui.enter_root_mode();
        ui.rotate_reset_choice(1);
        assert_eq!(ui.reset_choice(), ResetAllChoice::Reset);
        assert_eq!(ui.press_encoder(None), Some(UiAction::ResetConfirmed));
        assert_eq!(ui.page(), UiPage::Root);
        assert_eq!(ui.root_mode(), RootMode::ResetAll);
        assert_eq!(ui.selected_pad(), None);
    }

    #[test]
    fn display_model_explicitly_covers_root_overlays_prompts_and_editors() {
        let mut ui = UiController::new();
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::Root {
                highlighted: RootMode::Beats,
                selected_pad: None,
                current_song: None,
                song_dirty: false,
            }
        );
        ui.rotate_root(1);
        ui.enter_root_mode();
        assert_eq!(ui.display_model(false), UiDisplayModel::PatternSelectVoice);
        ui.press_voice(2);
        ui.rotate_pattern(2, 8, 3);
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::PatternEditor { pad: 2, cursor: 3 }
        );
        assert_eq!(
            ui.display_model(true),
            UiDisplayModel::PatternVolume {
                target: PatternVolumeTarget::Step { pad: 2, step: 2 },
            }
        );
        ui.rotate_pattern(2, 8, -3);
        assert_eq!(
            ui.press_encoder(Some(8)),
            Some(UiAction::Pattern(PatternEditorAction::AllMenuOpened))
        );
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::PatternAll {
                pad: 2,
                choice: PatternAllChoice::Cancel,
            }
        );

        ui.return_to_root(0, false);
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::Root {
                highlighted: RootMode::Pattern,
                selected_pad: Some(2),
                current_song: None,
                song_dirty: false,
            }
        );
        ui.rotate_root(-1);
        ui.enter_root_mode();
        assert_eq!(ui.display_model(false), UiDisplayModel::BeatsPad { pad: 2 });
        ui.press_voice(2);
        assert_eq!(ui.display_model(false), UiDisplayModel::BeatsGlobal);
        ui.press_voice(4);
        assert_eq!(ui.display_model(false), UiDisplayModel::BeatsPad { pad: 4 });

        ui.return_to_root(0, false);
        ui.rotate_root(2);
        ui.enter_root_mode();
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::SamplePad { pad: 4 }
        );
        ui.press_voice(4);
        assert_eq!(ui.display_model(false), UiDisplayModel::SampleSelectVoice);
        ui.press_voice(6);
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::SamplePad { pad: 6 }
        );

        ui.return_to_root(0, false);
        ui.rotate_root(1);
        ui.enter_root_mode();
        assert_eq!(ui.display_model(false), UiDisplayModel::Light);

        ui.return_to_root(0, false);
        ui.rotate_root(3);
        ui.enter_root_mode();
        ui.rotate_reset_choice(-100);
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::ResetAll {
                choice: ResetAllChoice::Cancel,
            }
        );
        ui.rotate_reset_choice(100);
        ui.rotate_reset_choice(100);
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::ResetAll {
                choice: ResetAllChoice::Reset,
            }
        );
    }

    #[test]
    fn song_slot_occupancy_and_library_display_are_compact_and_exact() {
        let first = SongSlot::from_number(1).unwrap();
        let middle = SongSlot::from_number(129).unwrap();
        let last = SongSlot::from_number(256).unwrap();
        let mut occupied = SongSlotOccupancy::empty();
        occupied.set(first, true);
        occupied.set(middle, true);
        occupied.set(last, true);
        assert!(occupied.contains(first));
        assert!(occupied.contains(middle));
        assert!(occupied.contains(last));
        assert_eq!(occupied.count(), 3);
        occupied.set(middle, false);
        assert!(!occupied.contains(middle));
        assert_eq!(occupied.count(), 2);
        assert_eq!(SongSlotOccupancy::from_words(*occupied.words()), occupied);

        let library = SongLibraryStatus {
            occupied,
            current_slot: Some(last),
            dirty: true,
        };
        let ui = UiController::new();
        assert_eq!(
            ui.display_model_with_library(false, library),
            UiDisplayModel::Root {
                highlighted: RootMode::Beats,
                selected_pad: None,
                current_song: Some(last),
                song_dirty: true,
            }
        );
    }

    #[test]
    fn root_save_is_immediate_and_unsaved_projects_can_be_redirected_to_save_as() {
        let mut ui = UiController::new();
        ui.rotate_root(RootMode::Save.index() as i32);
        let operation = SongStorageOperation::SaveCurrent;
        assert_eq!(ui.press_encoder(None), Some(UiAction::Song(operation)));
        assert_eq!(ui.song_status(), Some(SongUiStatus::Busy { operation }));
        assert_eq!(ui.encoder_target(true), UiEncoderTarget::SongStatus);

        let initial = SongSlot::from_number(17).unwrap();
        ui.open_save_as(Some(initial));
        assert_eq!(ui.page(), UiPage::Songs);
        assert_eq!(
            ui.songs_view(),
            SongsView::Browser {
                purpose: SongBrowserPurpose::SaveAs,
                slot: initial,
            }
        );
        assert_eq!(ui.song_status(), None);
    }

    #[test]
    fn songs_menu_clamps_slots_confirms_destructive_actions_and_emits_commands() {
        let mut ui = UiController::new();
        ui.rotate_root(RootMode::Songs.index() as i32);
        ui.enter_root_mode();
        assert_eq!(ui.page(), UiPage::Songs);
        assert_eq!(
            ui.songs_view(),
            SongsView::Operations {
                selected: SongMenuOperation::Load,
            }
        );
        assert_eq!(ui.encoder_target(false), UiEncoderTarget::Songs);

        ui.rotate_songs(100);
        assert_eq!(
            ui.songs_view(),
            SongsView::Operations {
                selected: SongMenuOperation::Delete,
            }
        );
        assert_eq!(ui.press_encoder(None), None);
        ui.rotate_songs(-100);
        assert_eq!(
            ui.songs_view(),
            SongsView::Browser {
                purpose: SongBrowserPurpose::Delete,
                slot: SongSlot::default(),
            }
        );
        ui.rotate_songs(999);
        let last = SongSlot::from_number(256).unwrap();
        assert_eq!(
            ui.songs_view(),
            SongsView::Browser {
                purpose: SongBrowserPurpose::Delete,
                slot: last,
            }
        );
        assert_eq!(ui.press_encoder(None), None);
        let operation = SongStorageOperation::Delete { slot: last };
        assert_eq!(
            ui.songs_view(),
            SongsView::Confirmation {
                operation,
                choice: SongConfirmChoice::Cancel,
            }
        );

        // Cancel is safe by default and returns to the same browser position.
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(
            ui.songs_view(),
            SongsView::Browser {
                purpose: SongBrowserPurpose::Delete,
                slot: last,
            }
        );
        assert_eq!(ui.press_encoder(None), None);
        ui.rotate_songs(1);
        assert_eq!(ui.press_encoder(None), Some(UiAction::Song(operation)));
        assert_eq!(ui.song_status(), Some(SongUiStatus::Busy { operation }));
        assert_eq!(
            ui.songs_view(),
            SongsView::Operations {
                selected: SongMenuOperation::Delete,
            }
        );
    }

    #[test]
    fn copy_uses_a_stored_source_and_separate_destination_without_wrapping() {
        let mut ui = UiController::new();
        ui.rotate_root(RootMode::Songs.index() as i32);
        ui.enter_root_mode();
        ui.rotate_songs(2);
        assert_eq!(ui.press_encoder(None), None);
        ui.rotate_songs(6);
        let source = SongSlot::from_number(7).unwrap();
        assert_eq!(
            ui.songs_view(),
            SongsView::Browser {
                purpose: SongBrowserPurpose::CopySource,
                slot: source,
            }
        );
        assert_eq!(ui.press_encoder(None), None);
        ui.rotate_songs(9);
        let destination = SongSlot::from_number(16).unwrap();
        assert_eq!(
            ui.songs_view(),
            SongsView::Browser {
                purpose: SongBrowserPurpose::CopyDestination { source },
                slot: destination,
            }
        );
        let operation = SongStorageOperation::Copy {
            source,
            destination,
        };
        assert_eq!(ui.press_encoder(None), None);
        let mut occupied = SongSlotOccupancy::empty();
        occupied.set(destination, true);
        assert_eq!(
            ui.display_model_with_library(
                false,
                SongLibraryStatus {
                    occupied,
                    current_slot: None,
                    dirty: true,
                },
            ),
            UiDisplayModel::SongConfirmation {
                operation,
                choice: SongConfirmChoice::Cancel,
                destination_occupied: true,
                live_song_dirty: true,
            }
        );
        ui.rotate_songs(1);
        assert_eq!(ui.press_encoder(None), Some(UiAction::Song(operation)));
    }

    #[test]
    fn song_confirmations_expose_overwrite_and_dirty_load_context() {
        let slot = SongSlot::from_number(1).unwrap();
        let mut occupied = SongSlotOccupancy::empty();
        occupied.set(slot, true);
        let library = SongLibraryStatus {
            occupied,
            current_slot: Some(slot),
            dirty: true,
        };

        let mut save_as = UiController::new();
        save_as.rotate_root(RootMode::Songs.index() as i32);
        save_as.enter_root_mode();
        save_as.rotate_songs(1);
        assert_eq!(save_as.press_encoder(None), None);
        assert_eq!(save_as.press_encoder(None), None);
        assert_eq!(
            save_as.display_model_with_library(false, library),
            UiDisplayModel::SongConfirmation {
                operation: SongStorageOperation::SaveAs { slot },
                choice: SongConfirmChoice::Cancel,
                destination_occupied: true,
                live_song_dirty: true,
            }
        );

        let mut load = UiController::new();
        load.rotate_root(RootMode::Songs.index() as i32);
        load.enter_root_mode();
        assert_eq!(load.press_encoder(None), None);
        assert_eq!(load.press_encoder(None), None);
        assert_eq!(
            load.display_model_with_library(false, library),
            UiDisplayModel::SongConfirmation {
                operation: SongStorageOperation::Load { slot },
                choice: SongConfirmChoice::Cancel,
                destination_occupied: false,
                live_song_dirty: true,
            }
        );
    }

    #[test]
    fn storage_status_distinguishes_newer_formats_corruption_busy_and_success() {
        let mut ui = UiController::new();
        let slot = SongSlot::from_number(42).unwrap();
        let unsupported = SongUiStatus::UnsupportedVersion {
            slot: Some(slot),
            found: 3,
            supported: 1,
        };
        ui.set_song_status(unsupported);
        assert_eq!(
            ui.display_model(false),
            UiDisplayModel::SongStatus {
                status: unsupported,
            }
        );
        assert_eq!(ui.encoder_target(true), UiEncoderTarget::SongStatus);
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(ui.song_status(), None);

        let unsupported_storage = SongUiStatus::UnsupportedStorage {
            found: 1,
            supported: 2,
        };
        ui.set_song_status(unsupported_storage);
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(ui.page(), UiPage::Songs);
        assert_eq!(
            ui.songs_view(),
            SongsView::Confirmation {
                operation: SongStorageOperation::Format,
                choice: SongConfirmChoice::Cancel,
            }
        );
        ui.rotate_songs(1);
        assert_eq!(
            ui.press_encoder(None),
            Some(UiAction::Song(SongStorageOperation::Format))
        );
        assert_eq!(
            ui.song_status(),
            Some(SongUiStatus::Busy {
                operation: SongStorageOperation::Format,
            })
        );

        let corrupt = SongUiStatus::Corrupt { slot: Some(slot) };
        ui.set_song_status(corrupt);
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(ui.song_status(), None);

        let operation = SongStorageOperation::Load { slot };
        ui.set_song_status(SongUiStatus::Busy { operation });
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(ui.song_status(), Some(SongUiStatus::Busy { operation }));
        ui.set_song_status(SongUiStatus::Formatting { percent: 37 });
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(
            ui.song_status(),
            Some(SongUiStatus::Formatting { percent: 37 })
        );
        ui.set_song_status(SongUiStatus::Success { operation });
        assert_eq!(ui.press_encoder(None), None);
        assert_eq!(ui.song_status(), None);
    }

    #[test]
    fn pattern_volume_modifier_targets_all_or_the_highlighted_trigger() {
        let mut ui = UiController::new();
        ui.rotate_root(1);
        ui.enter_root_mode();

        // Without a selected voice, Volume retains its ordinary master target.
        assert_eq!(
            ui.encoder_target(true),
            UiEncoderTarget::Volume(VolumeTarget::Global)
        );

        ui.press_voice(4);
        assert_eq!(
            ui.encoder_target(true),
            UiEncoderTarget::PatternVolume(PatternVolumeTarget::All { pad: 4 })
        );
        assert_eq!(
            ui.display_model(true),
            UiDisplayModel::PatternVolume {
                target: PatternVolumeTarget::All { pad: 4 },
            }
        );

        ui.rotate_pattern(4, 8, 3);
        assert_eq!(
            ui.encoder_target(true),
            UiEncoderTarget::PatternVolume(PatternVolumeTarget::Step { pad: 4, step: 2 })
        );
        assert_eq!(
            ui.display_model(true),
            UiDisplayModel::PatternVolume {
                target: PatternVolumeTarget::Step { pad: 4, step: 2 },
            }
        );

        // The modifier temporarily claims the encoder without moving the row.
        assert_eq!(ui.pattern_cursor(4), Some(3));
        assert_eq!(ui.encoder_target(false), UiEncoderTarget::Pattern(4));
        ui.rotate_pattern(4, 8, -3);
        assert_eq!(
            ui.press_encoder(Some(8)),
            Some(UiAction::Pattern(PatternEditorAction::AllMenuOpened))
        );
        assert_eq!(
            ui.encoder_target(true),
            UiEncoderTarget::PatternVolume(PatternVolumeTarget::All { pad: 4 })
        );
    }

    #[test]
    fn pattern_control_handles_rows_and_whole_map_choices() {
        let mut ui = UiController::new();
        assert_eq!(ui.press_pattern_control(None), None);

        ui.rotate_root(1);
        ui.enter_root_mode();
        assert_eq!(ui.page(), UiPage::Pattern);
        assert_eq!(ui.press_pattern_control(Some(4)), None);

        ui.press_voice(2);
        assert_eq!(
            ui.press_pattern_control(Some(4)),
            Some(UiAction::Pattern(PatternEditorAction::AllMenuOpened))
        );
        ui.rotate_pattern(2, 4, 2);
        assert_eq!(
            ui.press_pattern_control(Some(4)),
            Some(UiAction::Pattern(PatternEditorAction::SetAll {
                pad: 2,
                enabled: false,
            }))
        );

        ui.rotate_pattern(2, 4, 3);
        let mut encoder_ui = ui;
        assert_eq!(
            ui.press_pattern_control(Some(4)),
            Some(UiAction::Pattern(PatternEditorAction::Toggle {
                pad: 2,
                step: 2,
            }))
        );
        assert_eq!(
            encoder_ui.press_encoder(Some(4)),
            Some(UiAction::Pattern(PatternEditorAction::Toggle {
                pad: 2,
                step: 2,
            }))
        );
    }

    #[test]
    fn return_closes_every_page_and_pattern_confirmation_without_losing_root_cursor() {
        for mode in RootMode::ALL {
            let mut ui = UiController::new();
            ui.rotate_root(mode.index() as i32);
            ui.press_voice(3);
            ui.enter_root_mode();
            if mode == RootMode::Pattern {
                assert_eq!(
                    ui.press_encoder(Some(8)),
                    Some(UiAction::Pattern(PatternEditorAction::AllMenuOpened))
                );
                assert!(ui.pattern_all_menu().is_some());
            } else if mode == RootMode::ResetAll {
                ui.rotate_reset_choice(1);
                assert_eq!(ui.reset_choice(), ResetAllChoice::Reset);
            }

            let held = (1 << RETURN_KEY_INDEX) | (1 << VOLUME_KEY_INDEX) | (1 << 3);
            ui.return_to_root(held, true);
            assert_eq!(ui.page(), UiPage::Root);
            assert_eq!(ui.root_mode(), mode);
            assert_eq!(
                ui.selected_pad(),
                if mode == RootMode::Save {
                    None
                } else {
                    Some(3)
                }
            );
            assert_eq!(ui.pattern_all_menu(), None);
            assert_eq!(ui.reset_choice(), ResetAllChoice::Cancel);
            assert!(ui.key_suppressed(RETURN_KEY_INDEX));
            assert!(ui.key_suppressed(VOLUME_KEY_INDEX));
            assert!(ui.encoder_suppressed());

            ui.update_suppression(1 << VOLUME_KEY_INDEX, false);
            assert!(!ui.key_suppressed(RETURN_KEY_INDEX));
            assert!(ui.key_suppressed(VOLUME_KEY_INDEX));
            assert!(!ui.encoder_suppressed());
            ui.update_suppression(0, false);
            assert_eq!(ui.suppressed_keys(), 0);
            if mode != RootMode::Save {
                ui.return_to_root(0, false);
            }
            assert_eq!(ui.selected_pad(), None);
        }
    }

    #[test]
    fn persistent_pattern_menu_is_cancel_first_and_voice_switch_cancels_it() {
        let mut ui = UiController::new();
        ui.rotate_root(1);
        ui.enter_root_mode();
        ui.press_voice(1);
        assert_eq!(
            ui.press_encoder(Some(4)),
            Some(UiAction::Pattern(PatternEditorAction::AllMenuOpened))
        );
        assert_eq!(
            ui.pattern_all_menu().map(|menu| menu.choice),
            Some(PatternAllChoice::Cancel)
        );
        ui.rotate_pattern(1, 4, -100);
        assert_eq!(
            ui.pattern_all_menu().map(|menu| menu.choice),
            Some(PatternAllChoice::Cancel)
        );
        ui.rotate_pattern(1, 4, 1);
        assert_eq!(
            ui.pattern_all_menu().map(|menu| menu.choice),
            Some(PatternAllChoice::All)
        );
        ui.press_voice(5);
        assert_eq!(ui.selected_pad(), Some(5));
        assert_eq!(ui.pattern_all_menu(), None);

        assert_eq!(
            ui.press_encoder(Some(0)),
            Some(UiAction::Pattern(PatternEditorAction::AllMenuOpened))
        );
        ui.rotate_pattern(5, 0, 100);
        assert_eq!(
            ui.pattern_all_menu().map(|menu| menu.choice),
            Some(PatternAllChoice::None)
        );
        ui.rotate_pattern(5, 0, 1);
        assert_eq!(
            ui.pattern_all_menu().map(|menu| menu.choice),
            Some(PatternAllChoice::None)
        );
        assert_eq!(
            ui.press_encoder(Some(0)),
            Some(UiAction::Pattern(PatternEditorAction::SetAll {
                pad: 5,
                enabled: false,
            }))
        );
    }

    #[test]
    fn cancelling_mute_for_return_never_toggles_the_latch() {
        let mut button = MuteButtonState::new();
        let mut state = SharedState::default();
        assert!(button.press(MuteTarget::Pad(2), 100));
        assert!(state.begin_mute_gesture(MuteTarget::Pad(2)));
        assert_eq!(state.mute_indicator_active(MuteTarget::Pad(2)), Some(true));
        assert_eq!(button.cancel(), Some(MuteTarget::Pad(2)));
        assert_eq!(state.cancel_mute_gesture(), Some(MuteTarget::Pad(2)));
        assert_eq!(button.release(200), None);
        assert_eq!(state.latched_mute(MuteTarget::Pad(2)), Some(false));
        assert_eq!(state.mute_indicator_active(MuteTarget::Pad(2)), Some(false));
    }

    #[test]
    fn same_scan_return_cancels_mute_before_a_release_can_toggle_it() {
        let mut button = MuteButtonState::new();
        assert!(button.press(MuteTarget::Pad(4), 100));
        assert_eq!(
            resolve_mute_scan(&mut button, true, true, 200),
            Some(MuteScanAction::Cancel(MuteTarget::Pad(4)))
        );
        assert_eq!(button.active_target(), None);
        assert_eq!(button.release(201), None);

        assert!(button.press(MuteTarget::Global, 300));
        assert_eq!(
            resolve_mute_scan(&mut button, false, true, 599),
            Some(MuteScanAction::Release(MuteRelease {
                target: MuteTarget::Global,
                tapped: true,
            }))
        );
    }

    #[test]
    fn musical_reset_preserves_runtime_state_and_requests_one_block_boundary_release() {
        let mut state = SharedState::default();
        state.desired_beats[3] = 71;
        assert!(state.set_pad_sample(3, sample(5)));
        assert!(state.set_pattern_all(3, false));
        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::All { pad: 3 }, -20),
            Some(80)
        );
        assert_eq!(
            state.adjust_pattern_volume(PatternVolumeTarget::Step { pad: 3, step: 3 }, 10),
            Some(90)
        );
        assert!(state.begin_mute_gesture(MuteTarget::Pad(3)));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(3),
            tapped: true,
        }));
        assert_eq!(state.adjust_volume(VolumeTarget::Global, -25), Some(75));
        assert_eq!(state.adjust_volume(VolumeTarget::Pad(3), -40), Some(60));
        let preview = PreviewRequest::new(3, sample(5)).unwrap();
        assert_eq!(state.queue_preview(preview), None);
        state.base_interval_ms = 12_345;
        state.led_brightness_percent = 73;
        state.playback_frame = 9_876_543;
        state.latest_trigger_frames[3] = 123;
        state.underrun_count = 7;
        state.audio_load_transition_count = 11;
        let revision = state.pattern_revision;

        state.reset_musical_state();

        assert_eq!(state.desired_beats, [0; BEAT_PAD_COUNT]);
        assert_eq!(state.pad_samples(), &DEFAULT_PAD_SAMPLES);
        assert_eq!(state.take_preview(), None);
        assert_eq!(state.base_interval_ms, DEFAULT_BASE_INTERVAL_MS);
        assert_eq!(state.latest_trigger_frames, [0; BEAT_PAD_COUNT]);
        assert_eq!(state.latched_mute(MuteTarget::Global), Some(false));
        assert_eq!(state.latched_mute(MuteTarget::Pad(3)), Some(false));
        assert_eq!(state.volume_percent(VolumeTarget::Global), Some(100));
        assert_eq!(state.volume_percent(VolumeTarget::Pad(3)), Some(100));
        assert!(
            state
                .patterns
                .iter()
                .all(|pattern| { pattern.fill_state() == PatternFillState::Full })
        );
        assert!(state.trigger_volumes.iter().all(|volumes| {
            volumes.average_percent() == DEFAULT_TRIGGER_VOLUME_PERCENT
                && volumes.percent(0) == Some(DEFAULT_TRIGGER_VOLUME_PERCENT)
                && volumes.percent(PATTERN_BITS - 1) == Some(DEFAULT_TRIGGER_VOLUME_PERCENT)
        }));
        assert_eq!(state.pattern_revision, revision.wrapping_add(1));
        assert_eq!(state.take_pattern_dirty_mask(), BEAT_PAD_MASK);
        assert_eq!(state.led_brightness_percent, 73);
        assert_eq!(state.playback_frame, 9_876_543);
        assert_eq!(state.underrun_count, 7);
        assert_eq!(state.audio_load_transition_count, 11);
        assert!(state.take_release_all_request());
        assert!(!state.take_release_all_request());
    }

    #[test]
    fn release_all_cancels_preview_and_fades_primaries_over_exact_declick_window() {
        let mut sequencer = test_sequencer(KICK_WAV, HAT_WAV);
        let preview = PreviewRequest::new(2, DEFAULT_KICK_SAMPLE).unwrap();
        assert_eq!(sequencer.queue_preview(preview), None);
        let mut first = [0_u32; 1];
        sequencer.render(0, &mut first);
        assert_eq!(sequencer.active_voice_count_for_pad(2), Some(1));

        assert_eq!(sequencer.queue_preview(preview), None);
        sequencer.release_all_voices();
        let mut release = [0_u32; DECLICK_FRAMES as usize - 1];
        sequencer.render(1, &mut release);
        assert_eq!(sequencer.active_voice_count(), 1);
        let mut final_frame = [0_u32; 1];
        sequencer.render(DECLICK_FRAMES as u64, &mut final_frame);
        assert_eq!(sequencer.active_voice_count(), 0);
        assert_eq!(sequencer.active_fade_tail_count(), 0);
    }

    #[test]
    fn reset_release_freezes_silent_gain_while_targets_restore_to_full() {
        let mut sequencer = test_sequencer(KICK_WAV, HAT_WAV);
        let mut silent_pad = [100; BEAT_PAD_COUNT];
        silent_pad[0] = 0;
        sequencer.set_volumes(100, &silent_pad);
        for frame in 0..GAIN_RAMP_FRAMES {
            assert_eq!(
                sequencer.render_pcm_frame(u64::from(frame), &mut RenderReport::default()),
                0
            );
        }

        let allocation = VoiceAllocationState::settled(100, &silent_pad);
        assert!(sequencer.voices.start(
            0,
            DEFAULT_KICK_SAMPLE,
            StartPriority::Scheduled,
            allocation,
            &mut RenderReport::default(),
        ));
        assert_eq!(
            sequencer.render_pcm_frame(64, &mut RenderReport::default()),
            0
        );

        sequencer.set_volumes(100, &[100; BEAT_PAD_COUNT]);
        sequencer.release_all_voices();
        for frame in 0..DECLICK_FRAMES {
            assert_eq!(
                sequencer.render_pcm_frame(65 + u64::from(frame), &mut RenderReport::default()),
                0
            );
        }
        assert_eq!(sequencer.active_voice_count(), 0);
        assert_eq!(sequencer.pad_gains[0].current_q16(), 0);
    }

    #[test]
    fn reset_release_survives_emergency_policy_until_primaries_and_tails_finish() {
        let mut sequencer = test_sequencer(KICK_WAV, HAT_WAV);
        let volumes = [100; BEAT_PAD_COUNT];
        let allocation = VoiceAllocationState::settled(100, &volumes);
        let mut report = RenderReport::default();
        for pad in 0..2 {
            assert!(sequencer.voices.start(
                pad,
                DEFAULT_KICK_SAMPLE,
                StartPriority::Scheduled,
                allocation,
                &mut report,
            ));
        }
        let mut stolen = PlaybackVoice::idle();
        stolen.start(2, DEFAULT_KICK_SAMPLE, 99);
        sequencer
            .voices
            .preserve_stolen_voice(stolen, FADE_TAIL_COUNT as u8, &mut report);
        assert_eq!(sequencer.active_voice_count(), 2);
        assert_eq!(sequencer.active_fade_tail_count(), 1);

        sequencer.set_render_policy(RenderPolicy {
            max_primary_voices: 1,
            max_fade_tails: 0,
            preserve_stolen_fade_tails: false,
            release_excess_primaries: false,
            trim_excess_primaries: true,
            max_starts_per_pad: 1,
            allow_preview: false,
            dither_quality: DitherQuality::Coarse,
        });
        sequencer.release_all_voices();
        let mut first = [0_u32; DECLICK_FRAMES as usize - 1];
        let first_report = sequencer.render(0, &mut first);
        assert_eq!(first_report.load_shed_primary_count, 0);
        assert_eq!(first_report.load_shed_fade_tail_count, 0);
        assert_eq!(sequencer.active_voice_count(), 2);
        assert_eq!(sequencer.active_fade_tail_count(), 1);

        let mut last = [0_u32; 1];
        let last_report = sequencer.render(DECLICK_FRAMES as u64 - 1, &mut last);
        assert_eq!(last_report.load_shed_primary_count, 0);
        assert_eq!(last_report.load_shed_fade_tail_count, 0);
        assert_eq!(sequencer.active_voice_count(), 0);
        assert_eq!(sequencer.active_fade_tail_count(), 0);
    }

    #[test]
    fn song_slots_have_stable_one_based_numbers_and_unique_animal_names() {
        assert_eq!(SONG_SLOT_ANIMAL_NAMES.len(), SONG_SLOT_COUNT);
        assert_eq!(SongSlot::default().number(), 1);
        assert_eq!(SongSlot::from_number(1).unwrap().animal_name(), "Aardvark");
        assert_eq!(SongSlot::from_number(256).unwrap().animal_name(), "Zebu");
        assert_eq!(SongSlot::from_index(255).unwrap().storage_key(), 255);
        assert_eq!(SongSlot::from_number(0), None);
        assert_eq!(SongSlot::from_number(257), None);
        assert_eq!(SongSlot::from_index(SONG_SLOT_COUNT), None);
        for (index, &name) in SONG_SLOT_ANIMAL_NAMES.iter().enumerate() {
            assert!(!name.is_empty());
            assert_eq!(
                SongSlot::from_index(index).unwrap().number(),
                index as u16 + 1
            );
            assert!(
                SONG_SLOT_ANIMAL_NAMES[index + 1..]
                    .iter()
                    .all(|other| *other != name),
                "duplicate animal name {name}"
            );
        }
    }

    #[test]
    fn versioned_song_codec_round_trips_every_persistent_control() {
        let mut source = SharedState::default();
        assert!(source.set_base_interval_ms(71_073));
        assert!(source.set_desired_beats(3, MAX_BEAT_MULTIPLIER));
        assert!(source.set_pad_sample(3, sample(23)));
        assert_eq!(source.toggle_pattern_step(3, 255), Some(false));
        assert_eq!(
            source.adjust_pattern_volume(PatternVolumeTarget::Step { pad: 3, step: 255 }, -63,),
            Some(37)
        );
        assert_eq!(source.adjust_volume(VolumeTarget::Global, -25), Some(75));
        assert_eq!(source.adjust_volume(VolumeTarget::Pad(3), -40), Some(60));
        assert!(source.begin_mute_gesture(MuteTarget::Global));
        assert!(source.end_mute_gesture(MuteRelease {
            target: MuteTarget::Global,
            tapped: true,
        }));
        assert!(source.begin_mute_gesture(MuteTarget::Pad(3)));
        assert!(source.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(3),
            tapped: true,
        }));

        let song = StoredSongV1::snapshot(&source);
        let mut bytes = [0_u8; SONG_ENCODED_MAX_LEN];
        let encoded = encode_song_v1(&song, &mut bytes).unwrap();
        assert_eq!(&encoded[..4], &SONG_FORMAT_MAGIC);
        assert_eq!(
            u16::from_le_bytes([encoded[4], encoded[5]]),
            SONG_FORMAT_VERSION
        );
        assert!(encoded.len() < SONG_ENCODED_MAX_LEN);
        let decoded = decode_song(encoded).unwrap();
        assert_eq!(decoded, song);

        let mut destination = SharedState {
            led_brightness_percent: 17,
            playback_frame: 9_876_543,
            underrun_count: 12,
            audio_load_transition_count: 34,
            pattern_revision: 56,
            song_revision: 78,
            ..SharedState::default()
        };
        assert_eq!(
            destination.queue_preview(PreviewRequest::new(1, sample(4)).unwrap()),
            None
        );
        assert!(destination.begin_mute_gesture(MuteTarget::Pad(1)));

        decoded.apply_to(&mut destination).unwrap();

        assert_eq!(StoredSongV1::snapshot(&destination), song);
        assert_eq!(destination.led_brightness_percent, 17);
        assert_eq!(destination.playback_frame, 9_876_543);
        assert_eq!(destination.underrun_count, 12);
        assert_eq!(destination.audio_load_transition_count, 34);
        assert_eq!(destination.pattern_revision, 57);
        assert_eq!(destination.song_revision, 79);
        assert_eq!(destination.take_preview(), None);
        assert_eq!(destination.take_pattern_dirty_mask(), BEAT_PAD_MASK);
        assert!(destination.take_release_all_request());
        assert!(!destination.take_release_all_request());
        assert_eq!(destination.cancel_mute_gesture(), None);
    }

    #[test]
    fn default_song_v1_encoding_is_frozen() {
        const EXPECTED_FNV1A64: u64 = 0x4256_730a_616c_b5ef;

        fn fnv1a64(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }

        let mut bytes = [0_u8; SONG_ENCODED_MAX_LEN];
        let encoded = encode_song_v1(&StoredSongV1::default(), &mut bytes).unwrap();
        assert_eq!(
            (encoded.len(), fnv1a64(encoded)),
            (SONG_V1_DEFAULT_ENCODED_LEN, EXPECTED_FNV1A64)
        );
        assert_eq!(
            usize::from(u16::from_le_bytes([encoded[6], encoded[7]])),
            SONG_V1_DEFAULT_ENCODED_LEN - SONG_FORMAT_HEADER_LEN
        );
    }

    #[test]
    fn song_validation_rejects_each_bounded_field_and_apply_is_atomic() {
        let mut song = StoredSongV1 {
            base_interval_ms: MIN_BASE_INTERVAL_MS - 1,
            ..StoredSongV1::default()
        };
        assert_eq!(
            song.validate(),
            Err(SongValidationError::BaseIntervalTooShort {
                value: MIN_BASE_INTERVAL_MS - 1
            })
        );

        song = StoredSongV1::default();
        song.master_volume_percent = 101;
        assert_eq!(
            song.validate(),
            Err(SongValidationError::MasterVolumeOutOfRange { value: 101 })
        );

        song = StoredSongV1::default();
        song.pads[2].division = MAX_BEAT_MULTIPLIER + 1;
        assert_eq!(
            song.validate(),
            Err(SongValidationError::DivisionOutOfRange {
                pad: 2,
                value: MAX_BEAT_MULTIPLIER + 1
            })
        );

        song = StoredSongV1::default();
        song.pads[3].sample_id = SAMPLE_COUNT as u8;
        assert_eq!(
            song.validate(),
            Err(SongValidationError::SampleOutOfRange {
                pad: 3,
                value: SAMPLE_COUNT as u8
            })
        );

        song = StoredSongV1::default();
        song.pads[4].volume_percent = 200;
        assert_eq!(
            song.validate(),
            Err(SongValidationError::PadVolumeOutOfRange { pad: 4, value: 200 })
        );

        song = StoredSongV1::default();
        song.pads[5].trigger_levels[7][31] = 101;
        assert_eq!(
            song.validate(),
            Err(SongValidationError::TriggerVolumeOutOfRange {
                pad: 5,
                step: 255,
                value: 101
            })
        );

        let mut state = SharedState::default();
        assert!(state.set_base_interval_ms(2_000));
        state.led_brightness_percent = 91;
        let before = StoredSongV1::snapshot(&state);
        let revision = state.song_revision;
        assert_eq!(
            song.apply_to(&mut state),
            Err(SongValidationError::TriggerVolumeOutOfRange {
                pad: 5,
                step: 255,
                value: 101
            })
        );
        assert_eq!(StoredSongV1::snapshot(&state), before);
        assert_eq!(state.song_revision, revision);
        assert_eq!(state.led_brightness_percent, 91);
    }

    #[test]
    fn song_decoder_distinguishes_versions_corruption_and_semantic_errors() {
        let song = StoredSongV1::default();
        let mut bytes = [0_u8; SONG_ENCODED_MAX_LEN];
        let encoded_len = encode_song_v1(&song, &mut bytes).unwrap().len();

        for unsupported in [0_u16, SONG_FORMAT_VERSION + 1] {
            let mut versioned = bytes;
            versioned[4..6].copy_from_slice(&unsupported.to_le_bytes());
            assert_eq!(
                decode_song(&versioned[..encoded_len]),
                Err(SongDecodeError::UnsupportedVersion {
                    found: unsupported,
                    supported: SONG_FORMAT_VERSION,
                })
            );
        }

        assert_eq!(decode_song(&bytes[..7]), Err(SongDecodeError::Truncated));
        let mut bad_magic = bytes;
        bad_magic[..4].copy_from_slice(b"NOPE");
        assert_eq!(
            decode_song(&bad_magic[..encoded_len]),
            Err(SongDecodeError::BadMagic { found: *b"NOPE" })
        );

        let mut truncated = bytes;
        let longer = u16::from_le_bytes([truncated[6], truncated[7]]) + 1;
        truncated[6..8].copy_from_slice(&longer.to_le_bytes());
        assert_eq!(
            decode_song(&truncated[..encoded_len]),
            Err(SongDecodeError::Truncated)
        );

        let mut trailing = bytes;
        trailing[encoded_len] = 0xaa;
        let declared = u16::from_le_bytes([trailing[6], trailing[7]]);
        assert_eq!(
            decode_song(&trailing[..encoded_len + 1]),
            Err(SongDecodeError::LengthMismatch {
                declared,
                actual: usize::from(declared) + 1,
            })
        );

        let mut payload_with_junk = bytes;
        payload_with_junk[encoded_len] = 0xaa;
        payload_with_junk[6..8].copy_from_slice(&(declared + 1).to_le_bytes());
        assert_eq!(
            decode_song(&payload_with_junk[..encoded_len + 1]),
            Err(SongDecodeError::InvalidPayload)
        );

        let mut invalid_payload = [0_u8; SONG_FORMAT_HEADER_LEN + 1];
        invalid_payload[..4].copy_from_slice(&SONG_FORMAT_MAGIC);
        invalid_payload[4..6].copy_from_slice(&SONG_FORMAT_VERSION.to_le_bytes());
        invalid_payload[6..8].copy_from_slice(&1_u16.to_le_bytes());
        invalid_payload[8] = 0xff;
        assert_eq!(
            decode_song(&invalid_payload),
            Err(SongDecodeError::InvalidPayload)
        );

        let mut invalid_song = StoredSongV1::default();
        invalid_song.pads[7].sample_id = SAMPLE_COUNT as u8;
        let mut semantic = [0_u8; SONG_ENCODED_MAX_LEN];
        let payload_len =
            postcard::to_slice(&invalid_song, &mut semantic[SONG_FORMAT_HEADER_LEN..])
                .unwrap()
                .len();
        semantic[..4].copy_from_slice(&SONG_FORMAT_MAGIC);
        semantic[4..6].copy_from_slice(&SONG_FORMAT_VERSION.to_le_bytes());
        semantic[6..8].copy_from_slice(&(payload_len as u16).to_le_bytes());
        assert_eq!(
            decode_song(&semantic[..SONG_FORMAT_HEADER_LEN + payload_len]),
            Err(SongDecodeError::InvalidSong(
                SongValidationError::SampleOutOfRange {
                    pad: 7,
                    value: SAMPLE_COUNT as u8,
                }
            ))
        );
    }

    #[test]
    fn song_encoder_is_bounded_and_rejects_invalid_input() {
        let mut song = StoredSongV1 {
            base_interval_ms: u32::MAX,
            global_mute: true,
            ..StoredSongV1::default()
        };
        for pad in &mut song.pads {
            pad.division = MAX_BEAT_MULTIPLIER;
            pad.sample_id = (SAMPLE_COUNT - 1) as u8;
            pad.pattern = [0x55; PATTERN_BYTES];
            pad.mute = true;
        }
        let mut exact_budget = [0_u8; SONG_ENCODED_MAX_LEN];
        assert!(encode_song_v1(&song, &mut exact_budget).is_ok());
        let mut tiny = [0_u8; SONG_FORMAT_HEADER_LEN];
        assert_eq!(
            encode_song_v1(&song, &mut tiny),
            Err(SongEncodeError::BufferTooSmall)
        );
        let mut invalid = StoredSongV1::default();
        invalid.pads[0].volume_percent = 101;
        assert_eq!(
            encode_song_v1(&invalid, &mut exact_budget),
            Err(SongEncodeError::InvalidSong(
                SongValidationError::PadVolumeOutOfRange { pad: 0, value: 101 }
            ))
        );
    }

    #[test]
    fn song_revision_tracks_logical_persistent_edits_once() {
        let mut state = SharedState::default();
        assert_eq!(state.song_revision, 0);
        assert!(state.set_base_interval_ms(DEFAULT_BASE_INTERVAL_MS));
        assert!(state.set_desired_beats(0, 0));
        assert!(state.set_pad_sample(0, DEFAULT_PAD_SAMPLES[0]));
        assert_eq!(state.adjust_volume(VolumeTarget::Global, 0), Some(100));
        assert_eq!(state.song_revision, 0);

        assert!(state.set_base_interval_ms(2_000));
        assert_eq!(state.song_revision, 1);
        assert!(state.set_desired_beats(0, 4));
        assert_eq!(state.song_revision, 2);
        assert_eq!(state.toggle_pattern_step(0, 0), Some(false));
        assert_eq!(state.song_revision, 3);
        assert_eq!(state.adjust_volume(VolumeTarget::Pad(0), -1), Some(99));
        assert_eq!(state.song_revision, 4);
        assert!(state.begin_mute_gesture(MuteTarget::Pad(0)));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(0),
            tapped: false,
        }));
        assert_eq!(state.song_revision, 4);
        assert!(state.begin_mute_gesture(MuteTarget::Pad(0)));
        assert!(state.end_mute_gesture(MuteRelease {
            target: MuteTarget::Pad(0),
            tapped: true,
        }));
        assert_eq!(state.song_revision, 5);
        state.reset_musical_state();
        assert_eq!(state.song_revision, 6);
    }
}
