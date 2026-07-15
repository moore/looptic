#![no_std]

//! Platform-independent audio, sequencing, and UI state for LoopTic.
//!
//! The RP2040 firmware lives in `main.rs`. Keeping this module free of HAL
//! dependencies makes the timing and sample conversion code testable on a host.

pub const PAD_COUNT: usize = 12;
pub const SAMPLE_RATE: u32 = 22_050;
pub const AUDIO_BLOCK_FRAMES: usize = 128;
pub const MAX_RATE_HZ: u16 = 1_000;

const SAMPLE_COUNT: usize = 2;
const KICK_INDEX: usize = 0;
const OPEN_HAT_INDEX: usize = 1;
const PWM_QUANTIZED_MAX: u32 = 127;
const PWM_FRACTION_MASK: u32 = 0x1ff;
const PWM_COMMAND_BITS: u32 = 14;
const PWM_DITHER_CYCLES: u32 = 16;

/// Centered PWM command used while no PCM data is available.
pub const SILENCE_PWM_WORD: u32 = 64 | (63 << 7);

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
    pub rate_hz: u16,
    pub tick_ordinal: u64,
    pub next_frame: Option<u64>,
    pub voice: Voice,
}

impl PadState {
    const fn new(pad: usize) -> Self {
        Self {
            rate_hz: 0,
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
    dither: DitherEncoder,
}

impl<'a> Sequencer<'a> {
    pub fn new(kick: WavPcm16<'a>, open_hat: WavPcm16<'a>) -> Self {
        Self {
            samples: [kick, open_hat],
            pads: core::array::from_fn(PadState::new),
            dither: DitherEncoder::new(),
        }
    }

    pub fn pads(&self) -> &[PadState; PAD_COUNT] {
        &self.pads
    }

    /// Apply absolute rates at a render boundary.
    ///
    /// Changed rates are aligned to the global sample epoch and begin at the
    /// first tick strictly after `from_frame`. Unchanged pads retain phase.
    pub fn apply_rates(&mut self, rates: &[u16; PAD_COUNT], from_frame: u64) {
        for (pad, requested) in self.pads.iter_mut().zip(rates.iter().copied()) {
            let rate = requested.min(MAX_RATE_HZ);
            if pad.rate_hz == rate {
                continue;
            }

            pad.rate_hz = rate;
            if rate == 0 {
                pad.tick_ordinal = 0;
                pad.next_frame = None;
            } else {
                let ordinal = next_ordinal_after(from_frame, rate);
                pad.tick_ordinal = ordinal;
                pad.next_frame = Some(frame_for_tick(ordinal, rate));
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
            if pad
                .next_frame
                .is_some_and(|next| frame_has_reached(frame, next))
            {
                report.latest_visual_triggers[pad_index] = Some(frame);
                let sample_index = pad.voice.sample.index();
                if !sample_triggered[sample_index] {
                    pad.voice.trigger();
                    sample_triggered[sample_index] = true;
                    report.audible_trigger_counts[sample_index] =
                        report.audible_trigger_counts[sample_index].saturating_add(1);
                }

                pad.tick_ordinal = pad.tick_ordinal.wrapping_add(1);
                pad.next_frame = Some(frame_for_tick(pad.tick_ordinal, pad.rate_hz));
            }
        }

        let mut total = 0_i32;
        for pad in &mut self.pads {
            total = total.saturating_add(i32::from(pad.voice.next(&self.samples)));
        }
        saturating_i16(total)
    }
}

fn next_ordinal_after(frame: u64, rate: u16) -> u64 {
    let product = u128::from(frame) * u128::from(rate);
    let ordinal = product / u128::from(SAMPLE_RATE) + 1;
    ordinal as u64
}

fn frame_for_tick(ordinal: u64, rate: u16) -> u64 {
    if rate == 0 {
        return u64::MAX;
    }
    let numerator = u128::from(ordinal) * u128::from(SAMPLE_RATE);
    let denominator = u128::from(rate);
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
    pub desired_rates: [u16; PAD_COUNT],
    pub playback_frame: u64,
    pub latest_trigger_frames: [u64; PAD_COUNT],
    pub underrun_count: u32,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            desired_rates: [0; PAD_COUNT],
            playback_frame: 0,
            latest_trigger_frames: [0; PAD_COUNT],
            underrun_count: 0,
        }
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

/// Tracks the most recently pressed key that is still held.
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
        let mut newest = 0_u32;
        for pad in 0..PAD_COUNT {
            if self.held_mask & (1 << pad) != 0 && self.press_order[pad] >= newest {
                newest = self.press_order[pad];
                selected = Some(pad);
            }
        }
        selected
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

pub fn adjust_rate(current: u16, delta: i32, maximum: u16) -> u16 {
    let adjusted = i32::from(current).saturating_add(delta);
    adjusted.clamp(0, i32::from(maximum)) as u16
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
    fn scheduling_is_global_phase_aligned_and_rate_zero_stops_new_triggers() {
        let kick_bytes = wav(&[1, 2, 3], false);
        let hat_bytes = wav(&[4, 5, 6], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut rates = [0; PAD_COUNT];
        rates[0] = 1;
        sequencer.apply_rates(&rates, 0);
        assert_eq!(sequencer.pads()[0].next_frame, Some(22_050));

        rates[1] = 2;
        sequencer.apply_rates(&rates, 0);
        assert_eq!(sequencer.pads()[1].next_frame, Some(11_025));

        rates[2] = 1_000;
        sequencer.apply_rates(&rates, 0);
        assert_eq!(sequencer.pads()[2].next_frame, Some(23));

        rates[0] = 3;
        sequencer.apply_rates(&rates, 10_000);
        assert_eq!(sequencer.pads()[0].next_frame, Some(14_700));

        rates[0] = 0;
        sequencer.apply_rates(&rates, 10_001);
        assert_eq!(sequencer.pads()[0].next_frame, None);
    }

    #[test]
    fn scheduler_is_safe_across_playback_frame_wrap() {
        let kick_bytes = wav(&[1], false);
        let hat_bytes = wav(&[1], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut rates = [0; PAD_COUNT];
        rates[0] = 1_000;
        sequencer.apply_rates(&rates, u64::MAX - 10);
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
    fn rendering_suppresses_duplicate_samples_but_not_visuals() {
        let kick_bytes = wav(&[100, 50], false);
        let hat_bytes = wav(&[200, 100], false);
        let mut sequencer = Sequencer::new(
            WavPcm16::parse(&kick_bytes).unwrap(),
            WavPcm16::parse(&hat_bytes).unwrap(),
        );
        let mut rates = [0; PAD_COUNT];
        rates[0] = 1_000;
        rates[1] = 1_000;
        rates[6] = 1_000;
        sequencer.apply_rates(&rates, 0);

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
        let mut rates = [0; PAD_COUNT];
        rates[0] = 3;
        sequencer.apply_rates(&rates, 0);

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
    fn keys_debounce_and_selection_falls_back_to_previous_held_pad() {
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
        assert_eq!(selection.selected(), Some(4));
        selection.apply(KeyChanges {
            pressed: 0,
            released: 1 << 4,
        });
        assert_eq!(selection.selected(), Some(0));
    }

    #[test]
    fn rate_palette_and_led_helpers_are_bounded() {
        assert_eq!(adjust_rate(0, -1, MAX_RATE_HZ), 0);
        assert_eq!(adjust_rate(999, 10, MAX_RATE_HZ), MAX_RATE_HZ);
        assert_eq!(colorwheel(0), (255, 0, 0));
        assert_eq!(colorwheel(85), (0, 255, 0));
        assert_eq!(colorwheel(170), (0, 0, 255));
        assert!(!led_pulse_active(0, 0, 2_205));
        assert!(led_pulse_active(1_100, 1_000, 2_205));
        assert!(!led_pulse_active(3_205, 1_000, 2_205));
        assert!(led_pulse_active(5, u64::MAX - 4, 20));
    }
}
