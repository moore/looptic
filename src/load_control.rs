//! Predictable audio-load shedding for the RP2040 renderer.
//!
//! The controller observes the completed service time for each DMA block and
//! selects the policy for the following block.  It reacts well before the PIO
//! FIFO can underrun, then recovers slowly so quality does not chatter.

use crate::PRIMARY_VOICE_COUNT;

/// Nominal duration of one 128-frame block at the configured audio rate.
pub const AUDIO_BLOCK_BUDGET_US: u32 = 5_805;
/// Eight joined PIO FIFO words provide this much post-DMA handoff slack.
pub const AUDIO_FIFO_SLACK_US: u32 = 363;
/// A DMA launch gap beyond one block plus FIFO elasticity risks a PIO stall.
pub const AUDIO_LAUNCH_EMPTY_US: u32 = AUDIO_BLOCK_BUDGET_US + AUDIO_FIFO_SLACK_US;
/// A block above this mark also removes one currently mixed voice.
pub const AUDIO_PRESSURE_BLOCK_US: u32 = 4_350;
/// An instantaneous or smoothed load above this mark enters pressure mode.
pub const AUDIO_PRESSURE_SOFT_US: u32 = 3_775;
/// One block above this mark enters emergency mode.
pub const AUDIO_EMERGENCY_BLOCK_US: u32 = 5_225;
/// Recovery requires substantial headroom.
pub const AUDIO_RECOVERY_EWMA_US: u32 = 3_190;
pub const AUDIO_RECOVERY_WINDOW_MAX_US: u32 = 3_775;

const EWMA_SHIFT: u32 = 4;
const ROLLING_WINDOW_BLOCKS: u8 = 64;
const PRESSURE_RECOVERY_BLOCKS: u16 = 256;
const EMERGENCY_RECOVERY_BLOCKS: u16 = 512;
/// A musical hard bound that prevents an extreme grid from performing up to
/// 128 allocations per pad in one block. This still permits about 1,378
/// audible starts per second for each pad.
pub const FULL_QUALITY_MAX_STARTS_PER_PAD: u8 = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DitherQuality {
    #[default]
    Full,
    Coarse,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoadLevel {
    #[default]
    Normal,
    Pressure,
    Emergency,
    RecoveryDither,
    RecoveryTails,
    RecoveryStarts,
}

/// Hard work limits applied to one rendered block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderPolicy {
    pub max_primary_voices: u8,
    pub max_fade_tails: u8,
    pub preserve_stolen_fade_tails: bool,
    pub release_excess_primaries: bool,
    pub trim_excess_primaries: bool,
    pub max_starts_per_pad: u8,
    pub allow_preview: bool,
    pub dither_quality: DitherQuality,
}

impl RenderPolicy {
    pub const FULL: Self = Self {
        max_primary_voices: PRIMARY_VOICE_COUNT as u8,
        max_fade_tails: crate::FADE_TAIL_COUNT as u8,
        preserve_stolen_fade_tails: true,
        release_excess_primaries: false,
        trim_excess_primaries: false,
        max_starts_per_pad: FULL_QUALITY_MAX_STARTS_PER_PAD,
        allow_preview: true,
        dither_quality: DitherQuality::Full,
    };
}

impl Default for RenderPolicy {
    fn default() -> Self {
        Self::FULL
    }
}

/// Stateful, hysteretic controller for the next block's render policy.
#[derive(Clone, Copy, Debug)]
pub struct AudioLoadController {
    level: LoadLevel,
    primary_limit: u8,
    ewma_q4_us: u32,
    rolling_max_us: u32,
    rolling_blocks: u8,
    healthy_blocks: u16,
    observed_blocks: u32,
    last_underrun_count: u32,
}

impl AudioLoadController {
    pub const fn new() -> Self {
        Self {
            level: LoadLevel::Normal,
            primary_limit: PRIMARY_VOICE_COUNT as u8,
            ewma_q4_us: 0,
            rolling_max_us: 0,
            rolling_blocks: 0,
            healthy_blocks: 0,
            observed_blocks: 0,
            last_underrun_count: 0,
        }
    }

    pub const fn level(&self) -> LoadLevel {
        self.level
    }

    pub const fn ewma_us(&self) -> u32 {
        self.ewma_q4_us >> EWMA_SHIFT
    }

    /// Maximum service time in the current tumbling 64-block window.
    pub const fn window_max_us(&self) -> u32 {
        self.rolling_max_us
    }

    pub const fn observed_blocks(&self) -> u32 {
        self.observed_blocks
    }

    pub const fn policy(&self) -> RenderPolicy {
        match self.level {
            LoadLevel::Normal => RenderPolicy::FULL,
            LoadLevel::Pressure => RenderPolicy {
                max_primary_voices: self.primary_limit,
                max_fade_tails: crate::FADE_TAIL_COUNT as u8,
                preserve_stolen_fade_tails: false,
                release_excess_primaries: true,
                trim_excess_primaries: false,
                max_starts_per_pad: 1,
                allow_preview: false,
                dither_quality: DitherQuality::Coarse,
            },
            LoadLevel::Emergency => RenderPolicy {
                max_primary_voices: self.primary_limit,
                max_fade_tails: 0,
                preserve_stolen_fade_tails: false,
                release_excess_primaries: false,
                trim_excess_primaries: true,
                max_starts_per_pad: 1,
                allow_preview: false,
                dither_quality: DitherQuality::Coarse,
            },
            LoadLevel::RecoveryDither => RenderPolicy {
                max_primary_voices: self.primary_limit,
                max_fade_tails: 0,
                preserve_stolen_fade_tails: false,
                release_excess_primaries: false,
                trim_excess_primaries: false,
                max_starts_per_pad: 1,
                allow_preview: false,
                dither_quality: DitherQuality::Full,
            },
            LoadLevel::RecoveryTails => RenderPolicy {
                max_primary_voices: self.primary_limit,
                max_fade_tails: crate::FADE_TAIL_COUNT as u8,
                preserve_stolen_fade_tails: true,
                release_excess_primaries: false,
                trim_excess_primaries: false,
                max_starts_per_pad: 1,
                allow_preview: false,
                dither_quality: DitherQuality::Full,
            },
            LoadLevel::RecoveryStarts => RenderPolicy {
                max_primary_voices: self.primary_limit,
                max_fade_tails: crate::FADE_TAIL_COUNT as u8,
                preserve_stolen_fade_tails: true,
                release_excess_primaries: false,
                trim_excess_primaries: false,
                max_starts_per_pad: FULL_QUALITY_MAX_STARTS_PER_PAD,
                allow_preview: false,
                dither_quality: DitherQuality::Full,
            },
        }
    }

    /// Observe a completed block and return the policy for the next one.
    ///
    /// `peak_primary_voices` is the largest number mixed during the measured
    /// block. `underrun_count` is cumulative; any increase is an emergency.
    pub fn observe(
        &mut self,
        service_us: u32,
        peak_primary_voices: u8,
        underrun_count: u32,
    ) -> RenderPolicy {
        self.observe_with_cadence(service_us, 0, 0, peak_primary_voices, underrun_count)
    }

    /// Observe a completed service cycle including DMA handoff timing.
    pub fn observe_with_cadence(
        &mut self,
        service_us: u32,
        launch_gap_us: u32,
        handoff_us: u32,
        peak_primary_voices: u8,
        underrun_count: u32,
    ) -> RenderPolicy {
        self.observed_blocks = self.observed_blocks.saturating_add(1);
        let sample_q4 = service_us.saturating_mul(1 << EWMA_SHIFT);
        if self.ewma_q4_us == 0 {
            self.ewma_q4_us = sample_q4;
        } else {
            let difference = i64::from(sample_q4) - i64::from(self.ewma_q4_us);
            self.ewma_q4_us =
                (i64::from(self.ewma_q4_us) + (difference >> EWMA_SHIFT)).max(0) as u32;
        }

        self.rolling_max_us = self.rolling_max_us.max(service_us);
        self.rolling_blocks = self.rolling_blocks.saturating_add(1);

        let new_underrun = underrun_count != self.last_underrun_count;
        self.last_underrun_count = underrun_count;
        let cadence_emergency = (launch_gap_us != 0 && launch_gap_us > AUDIO_LAUNCH_EMPTY_US)
            || (handoff_us != 0 && handoff_us > AUDIO_FIFO_SLACK_US);
        let emergency = new_underrun || cadence_emergency || service_us >= AUDIO_EMERGENCY_BLOCK_US;
        let pressure =
            service_us >= AUDIO_PRESSURE_SOFT_US || self.ewma_us() >= AUDIO_PRESSURE_SOFT_US;

        if emergency {
            self.level = LoadLevel::Emergency;
            self.primary_limit = self
                .primary_limit
                .min(peak_primary_voices.saturating_sub(2).max(1));
            self.healthy_blocks = 0;
        } else if pressure {
            if self.level == LoadLevel::Emergency {
                // Do not plateau just below the emergency threshold. Keep
                // contracting until the current (not merely smoothed) service
                // time has meaningful headroom for the thread executor.
                if service_us >= AUDIO_PRESSURE_SOFT_US {
                    self.primary_limit = self
                        .primary_limit
                        .min(peak_primary_voices.saturating_sub(1).max(1));
                }
            } else {
                self.level = LoadLevel::Pressure;
                let requested = if service_us >= AUDIO_PRESSURE_BLOCK_US {
                    peak_primary_voices.saturating_sub(1).max(1)
                } else {
                    peak_primary_voices.max(1)
                };
                self.primary_limit = self.primary_limit.min(requested);
            }
            self.healthy_blocks = 0;
        } else {
            let healthy = self.ewma_us() < AUDIO_RECOVERY_EWMA_US
                && self.rolling_max_us < AUDIO_RECOVERY_WINDOW_MAX_US;
            if healthy {
                self.healthy_blocks = self.healthy_blocks.saturating_add(1);
            } else {
                self.healthy_blocks = 0;
            }

            let recovery_blocks = match self.level {
                LoadLevel::Normal => 0,
                LoadLevel::Pressure => PRESSURE_RECOVERY_BLOCKS,
                LoadLevel::Emergency => EMERGENCY_RECOVERY_BLOCKS,
                LoadLevel::RecoveryDither
                | LoadLevel::RecoveryTails
                | LoadLevel::RecoveryStarts => PRESSURE_RECOVERY_BLOCKS,
            };
            if recovery_blocks != 0 && self.healthy_blocks >= recovery_blocks {
                self.healthy_blocks = 0;
                match self.level {
                    LoadLevel::Emergency => self.level = LoadLevel::Pressure,
                    LoadLevel::Pressure => {
                        self.primary_limit = self
                            .primary_limit
                            .saturating_add(1)
                            .min(PRIMARY_VOICE_COUNT as u8);
                        if self.primary_limit == PRIMARY_VOICE_COUNT as u8 {
                            self.level = LoadLevel::RecoveryDither;
                        }
                    }
                    LoadLevel::RecoveryDither => self.level = LoadLevel::RecoveryTails,
                    LoadLevel::RecoveryTails => self.level = LoadLevel::RecoveryStarts,
                    LoadLevel::RecoveryStarts => self.level = LoadLevel::Normal,
                    LoadLevel::Normal => {}
                }
            }
        }

        if self.rolling_blocks >= ROLLING_WINDOW_BLOCKS {
            self.rolling_blocks = 0;
            self.rolling_max_us = 0;
        }
        self.policy()
    }
}

impl Default for AudioLoadController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_policy_preserves_full_quality() {
        let mut controller = AudioLoadController::new();
        let policy = controller.observe(1_000, 24, 0);
        assert_eq!(controller.level(), LoadLevel::Normal);
        assert_eq!(policy, RenderPolicy::FULL);
    }

    #[test]
    fn pressure_sheds_optional_work_and_caps_growth() {
        let mut controller = AudioLoadController::new();
        let policy = controller.observe(AUDIO_PRESSURE_BLOCK_US, 15, 0);
        assert_eq!(controller.level(), LoadLevel::Pressure);
        assert_eq!(policy.max_primary_voices, 14);
        assert_eq!(policy.max_fade_tails, crate::FADE_TAIL_COUNT as u8);
        assert!(!policy.preserve_stolen_fade_tails);
        assert!(policy.release_excess_primaries);
        assert!(!policy.trim_excess_primaries);
        assert_eq!(policy.max_starts_per_pad, 1);
        assert!(!policy.allow_preview);
        assert_eq!(policy.dither_quality, DitherQuality::Coarse);
    }

    #[test]
    fn underrun_enters_emergency_and_reduces_two_voices() {
        let mut controller = AudioLoadController::new();
        let policy = controller.observe(1_000, 15, 1);
        assert_eq!(controller.level(), LoadLevel::Emergency);
        assert_eq!(policy.max_primary_voices, 13);
    }

    #[test]
    fn late_dma_launch_or_handoff_enters_emergency() {
        let mut controller = AudioLoadController::new();
        let policy = controller.observe_with_cadence(1_000, AUDIO_LAUNCH_EMPTY_US + 1, 1, 10, 0);
        assert_eq!(controller.level(), LoadLevel::Emergency);
        assert_eq!(policy.max_primary_voices, 8);

        let mut controller = AudioLoadController::new();
        controller.observe_with_cadence(1_000, 1, AUDIO_FIFO_SLACK_US + 1, 10, 0);
        assert_eq!(controller.level(), LoadLevel::Emergency);
    }

    #[test]
    fn soft_service_threshold_enters_pressure_before_a_long_block() {
        let mut controller = AudioLoadController::new();
        for _ in 0..64 {
            controller.observe(AUDIO_PRESSURE_SOFT_US + 100, 12, 0);
            if controller.level() == LoadLevel::Pressure {
                break;
            }
        }
        assert_eq!(controller.level(), LoadLevel::Pressure);
        assert_eq!(controller.policy().max_primary_voices, 12);
    }

    #[test]
    fn pressure_recovery_is_deliberately_slow() {
        let mut controller = AudioLoadController::new();
        controller.observe(AUDIO_PRESSURE_BLOCK_US, 10, 0);
        assert_eq!(controller.policy().max_primary_voices, 9);

        // Clear the previous rolling maximum, then accumulate healthy blocks.
        for _ in 0..usize::from(ROLLING_WINDOW_BLOCKS) + usize::from(PRESSURE_RECOVERY_BLOCKS) {
            controller.observe(1_000, 9, 0);
        }
        assert_eq!(controller.level(), LoadLevel::Pressure);
        assert_eq!(controller.policy().max_primary_voices, 10);
    }

    #[test]
    fn emergency_recovers_to_pressure_not_directly_to_full() {
        let mut controller = AudioLoadController::new();
        controller.observe(AUDIO_EMERGENCY_BLOCK_US, 8, 0);
        for _ in 0..(ROLLING_WINDOW_BLOCKS as usize + EMERGENCY_RECOVERY_BLOCKS as usize) {
            controller.observe(1_000, 6, 0);
        }
        assert_eq!(controller.level(), LoadLevel::Pressure);
    }

    #[test]
    fn emergency_keeps_contracting_while_current_blocks_are_slow() {
        let mut controller = AudioLoadController::new();
        controller.observe(AUDIO_EMERGENCY_BLOCK_US, 15, 0);
        assert_eq!(controller.level(), LoadLevel::Emergency);
        assert_eq!(controller.policy().max_primary_voices, 13);

        controller.observe(AUDIO_EMERGENCY_BLOCK_US - 1, 13, 0);
        assert_eq!(controller.level(), LoadLevel::Emergency);
        assert_eq!(controller.policy().max_primary_voices, 12);

        controller.observe(AUDIO_PRESSURE_SOFT_US, 12, 0);
        assert_eq!(controller.policy().max_primary_voices, 11);

        // A low current sample does not keep cutting merely because the EWMA
        // is still elevated from the preceding slow blocks.
        controller.observe(1_000, 11, 0);
        assert_eq!(controller.policy().max_primary_voices, 11);
    }

    #[test]
    fn full_capacity_recovery_restores_dither_before_optional_work() {
        let mut controller = AudioLoadController::new();
        controller.observe(AUDIO_PRESSURE_BLOCK_US, PRIMARY_VOICE_COUNT as u8, 0);
        assert_eq!(controller.policy().max_primary_voices, 23);

        for _ in 0..usize::from(ROLLING_WINDOW_BLOCKS) + usize::from(PRESSURE_RECOVERY_BLOCKS) {
            controller.observe(1_000, 23, 0);
        }
        assert_eq!(controller.level(), LoadLevel::RecoveryDither);
        let recovery = controller.policy();
        assert_eq!(recovery.max_primary_voices, PRIMARY_VOICE_COUNT as u8);
        assert_eq!(recovery.dither_quality, DitherQuality::Full);
        assert_eq!(recovery.max_fade_tails, 0);
        assert!(!recovery.allow_preview);

        for _ in 0..PRESSURE_RECOVERY_BLOCKS {
            controller.observe(1_000, PRIMARY_VOICE_COUNT as u8, 0);
        }
        assert_eq!(controller.level(), LoadLevel::RecoveryTails);
        assert!(controller.policy().preserve_stolen_fade_tails);
        assert_eq!(
            controller.policy().max_fade_tails,
            crate::FADE_TAIL_COUNT as u8
        );
        assert_eq!(controller.policy().max_starts_per_pad, 1);

        for _ in 0..PRESSURE_RECOVERY_BLOCKS {
            controller.observe(1_000, PRIMARY_VOICE_COUNT as u8, 0);
        }
        assert_eq!(controller.level(), LoadLevel::RecoveryStarts);
        assert_eq!(
            controller.policy().max_starts_per_pad,
            FULL_QUALITY_MAX_STARTS_PER_PAD
        );
        assert!(!controller.policy().allow_preview);

        for _ in 0..PRESSURE_RECOVERY_BLOCKS {
            controller.observe(1_000, PRIMARY_VOICE_COUNT as u8, 0);
        }
        assert_eq!(controller.level(), LoadLevel::Normal);
        assert_eq!(controller.policy(), RenderPolicy::FULL);
    }
}
