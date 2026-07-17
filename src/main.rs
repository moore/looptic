//! LoopTic firmware for the Adafruit MacroPad RP2040.
//!
//! The MacroPadSynthPlug is a passive PWM-to-line-out filter. Audio is therefore
//! generated directly on GP20 by a PIO state machine and fed from DMA. The PIO
//! program below is an adaptation of Raspberry Pi's `pwm_one_bit_dither`
//! program from pico-extras.

#![no_std]
#![no_main]

use core::cell::RefCell;
use core::fmt::Write as _;
use core::mem;
use core::sync::atomic::{AtomicBool, Ordering};

use cortex_m_rt::entry;
use embassy_executor::{Executor, InterruptExecutor};
use embassy_rp::Peri;
use embassy_rp::bind_interrupts;
use embassy_rp::dma;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::interrupt::{InterruptExt, Priority};
use embassy_rp::peripherals::{
    DMA_CH0, DMA_CH1, PIN_22, PIN_23, PIN_24, PIN_26, PIN_27, PIO0, PIO1, SPI1,
};
use embassy_rp::pio::program::pio_asm;
use embassy_rp::pio::{
    Config as PioConfig, Direction as PioDirection, FifoJoin,
    InterruptHandler as PioInterruptHandler, Pio, ShiftConfig, ShiftDirection, StateMachine,
};
use embassy_rp::pio_programs::rotary_encoder::{Direction, PioEncoder, PioEncoderProgram};
use embassy_rp::pio_programs::ws2812::{Grb, PioWs2812, PioWs2812Program};
use embassy_rp::spi::{self, Spi};
use embassy_rp::{interrupt, peripherals};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Delay, Duration, Instant, Timer, with_deadline};
use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_6X10};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use fixed::types::U24F8;
use heapless::String;
use looptic::load_control::{AudioLoadController, LoadLevel, RenderPolicy};
use looptic::{
    AUDIO_BLOCK_FRAMES, BEAT_PAD_COUNT, EncoderAcceleration, EncoderTarget, HeldPadSelection,
    KEY_COUNT, KeyDebouncer, MUTE_KEY_INDEX, MuteButtonState, MuteTarget, PatternAllChoice,
    PatternEditorAction, PatternEditorState, PatternFillState, SAMPLE_COUNT, SAMPLE_KEY_INDEX,
    SAMPLE_RATE, SILENCE_PWM_WORD, Sequencer, SharedState, VOLUME_KEY_INDEX, VolumeControlState,
    VolumeTarget, adjust_sample_selection, apply_encoder_delta, colorwheel, led_pulse_active,
    mute_led_color, pattern_button_allowed, pattern_window_start, sample_assets, sample_led_color,
    scale_color, volume_led_color,
};
use sh1106::Builder;
use sh1106::mode::GraphicsMode;
use smart_leds::RGB8;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    PIO1_IRQ_0 => PioInterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

type Shared = Mutex<CriticalSectionRawMutex, RefCell<SharedState>>;
type UiShared = Mutex<CriticalSectionRawMutex, RefCell<UiState>>;

#[derive(Clone, Copy, Default)]
struct UiState {
    selected_pad: Option<usize>,
    encoder_pressed: bool,
    volume_pressed: bool,
    sample_pressed: bool,
    volume_target: Option<VolumeTarget>,
    pattern_editor: PatternEditorState,
}

struct OledResources {
    spi: Peri<'static, SPI1>,
    sck: Peri<'static, PIN_26>,
    mosi: Peri<'static, PIN_27>,
    cs: Peri<'static, PIN_22>,
    reset: Peri<'static, PIN_23>,
    dc: Peri<'static, PIN_24>,
}

#[derive(Clone, Copy)]
struct AudioServiceObservation {
    service_us: u32,
    render_us: u32,
    dma_cadence_us: u32,
    handoff_us: u32,
    peak_primary_voices: u8,
}

#[derive(Clone, Copy)]
struct AudioServiceMetrics {
    observation: AudioServiceObservation,
    observed_load_level: LoadLevel,
    observed_policy: RenderPolicy,
    next_load_level: LoadLevel,
    next_policy: RenderPolicy,
    load_ewma_us: u32,
    window_max_us: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioDisplayMetrics {
    level: LoadLevel,
    active_voices: u8,
    voice_limit: u8,
    last_service_us: u32,
    max_service_us: u32,
    last_render_us: u32,
    max_render_us: u32,
    max_dma_cadence_us: u32,
    deadline_misses: u32,
    underruns: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DisplayStateKey {
    target: EncoderTarget,
    displayed_value: u32,
    pattern_revision: u32,
    pattern_all_choice: Option<PatternAllChoice>,
    diagnostics: Option<AudioDisplayMetrics>,
}

static SHARED: StaticCell<Shared> = StaticCell::new();
static UI_SHARED: StaticCell<UiShared> = StaticCell::new();
static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
static EXECUTOR_LOW: StaticCell<Executor> = StaticCell::new();
static AUDIO_DMA_FRONT: StaticCell<[u32; AUDIO_BLOCK_FRAMES]> = StaticCell::new();
static AUDIO_DMA_BACK: StaticCell<[u32; AUDIO_BLOCK_FRAMES]> = StaticCell::new();
static FATAL_FAULT: AtomicBool = AtomicBool::new(false);

const AUDIO_CLOCK_DIVIDER_BITS: u32 = 667;
const KEY_SCAN_INTERVAL: Duration = Duration::from_millis(1);
const DISPLAY_INTERVAL: Duration = Duration::from_millis(34);
const LED_INTERVAL: Duration = Duration::from_millis(5);
const AUDIO_BLOCK_DEADLINE_US: u64 = looptic::load_control::AUDIO_BLOCK_BUDGET_US as u64;

/// Copyright (c) 2020 Raspberry Pi (Trading) Ltd.
///
/// SPDX-License-Identifier: BSD-3-Clause
///
/// This is Raspberry Pi's one-bit-dither PWM program with an initial jump
/// added so it starts correctly regardless of where Embassy relocates it.
fn configure_audio_pio(
    pio: Peri<'static, PIO0>,
    audio_pin: Peri<'static, peripherals::PIN_20>,
) -> StateMachine<'static, PIO0, 0> {
    let Pio {
        mut common,
        sm0: mut sm,
        ..
    } = Pio::new(pio, Irqs);

    let program = pio_asm!(
        r#"
        .side_set 1 opt

        jmp entry
        delay:
            nop [2]
        .wrap_target
            out pins, 1
        loops:
            mov x, isr side 1
        loop1:
            jmp x--, loop1
            mov x, y side 0
        loop0:
            jmp x--, loop0
            jmp !osre, delay
        entry:
            pull
            out isr, 7
            out y, 7
        .wrap
        "#,
    );

    let loaded = common.load_program(&program.program);
    let audio_pin = common.make_pio_pin(audio_pin);
    sm.set_pins(Level::Low, &[&audio_pin]);
    sm.set_pin_dirs(PioDirection::Out, &[&audio_pin]);

    let mut config = PioConfig::default();
    config.use_program(&loaded, &[&audio_pin]);
    config.set_out_pins(&[&audio_pin]);
    config.clock_divider = U24F8::from_bits(AUDIO_CLOCK_DIVIDER_BITS);
    config.fifo_join = FifoJoin::TxOnly;
    config.shift_out = ShiftConfig {
        auto_fill: false,
        threshold: 30,
        direction: ShiftDirection::Right,
    };
    sm.set_config(&config);
    sm
}

#[embassy_executor::task]
async fn audio_task(
    mut sm: StateMachine<'static, PIO0, 0>,
    dma_peripheral: Peri<'static, DMA_CH0>,
    mut sequencer: Sequencer<'static>,
    shared: &'static Shared,
) {
    let mut dma = dma::Channel::new(dma_peripheral, Irqs);
    let mut front = AUDIO_DMA_FRONT.init([SILENCE_PWM_WORD; AUDIO_BLOCK_FRAMES]);
    let mut back = AUDIO_DMA_BACK.init([SILENCE_PWM_WORD; AUDIO_BLOCK_FRAMES]);

    let mut front_start = 0_u64;
    let (
        initial_beats,
        initial_pad_samples,
        initial_base_interval,
        initial_mute_mask,
        initial_global_volume,
        initial_pad_volumes,
    ) = shared.lock(|state| {
        let state = state.borrow();
        (
            state.desired_beats,
            *state.pad_samples(),
            state.base_interval_ms,
            state.effective_mute_mask(),
            state.global_volume_percent(),
            *state.pad_volume_percents(),
        )
    });
    sequencer.apply_timing(&initial_beats, initial_base_interval, front_start);
    sequencer.set_pad_samples(&initial_pad_samples);
    sequencer.set_mute_mask(initial_mute_mask);
    sequencer.set_volumes(initial_global_volume, &initial_pad_volumes);
    sync_pattern_updates(shared, &mut sequencer);
    let render_started = Instant::now();
    let report = sequencer.render(front_start, front);
    let render_us = Instant::now()
        .saturating_duration_since(render_started)
        .as_micros();
    publish_render(shared, &report, render_us);

    // Joined TX mode provides eight words of elasticity between DMA completions.
    for _ in 0..8 {
        sm.tx().push(SILENCE_PWM_WORD);
    }
    // Ignore the intentional empty-FIFO condition that existed before startup.
    let _ = sm.tx().stalled();
    sm.set_enable(true);

    let mut load_controller = AudioLoadController::new();
    let mut policy = RenderPolicy::FULL;
    let mut previous_observation: Option<AudioServiceObservation> = None;
    let mut pending_metrics: Option<AudioServiceMetrics> = None;
    let mut previous_dma_started: Option<Instant> = None;
    let mut previous_handoff_started: Option<Instant> = None;
    let mut underrun_count = 0_u32;

    loop {
        let service_started = Instant::now();
        if let Some(observation) = previous_observation.take() {
            policy = load_controller.observe_with_cadence(
                observation.service_us,
                observation.dma_cadence_us,
                observation.handoff_us,
                observation.peak_primary_voices,
                underrun_count,
            );
            if let Some(metrics) = pending_metrics.as_mut() {
                metrics.next_load_level = load_controller.level();
                metrics.next_policy = policy;
                metrics.load_ewma_us = load_controller.ewma_us();
                metrics.window_max_us = load_controller.window_max_us();
            }
        }
        sequencer.set_render_policy(policy);

        let transfer = sm.tx().dma_push(&mut dma, front, false);
        // `dma_push` writes CTRL_TRIG internally; the post-call timestamp is a
        // conservative approximation of the instant DMA actually started.
        let dma_started = Instant::now();
        let dma_cadence_us = previous_dma_started.map_or(0, |previous| {
            duration_us_u32(dma_started.saturating_duration_since(previous))
        });
        let handoff_us = previous_handoff_started.map_or(0, |previous| {
            duration_us_u32(dma_started.saturating_duration_since(previous))
        });
        previous_dma_started = Some(dma_started);

        let back_start = front_start.wrapping_add(AUDIO_BLOCK_FRAMES as u64);
        let (beats, pad_samples, preview, base_interval, mute_mask, global_volume, pad_volumes) =
            shared.lock(|state| {
                let mut state = state.borrow_mut();
                if let Some(metrics) = pending_metrics.take() {
                    record_audio_service_metrics(&mut state, metrics);
                }
                state.playback_frame = front_start;
                (
                    state.desired_beats,
                    *state.pad_samples(),
                    state.take_preview(),
                    state.base_interval_ms,
                    state.effective_mute_mask(),
                    state.global_volume_percent(),
                    *state.pad_volume_percents(),
                )
            });
        sequencer.apply_timing(&beats, base_interval, back_start);
        sequencer.set_pad_samples(&pad_samples);
        sequencer.set_mute_mask(mute_mask);
        sequencer.set_volumes(global_volume, &pad_volumes);
        if let Some(preview) = preview {
            let _ = sequencer.queue_preview(preview);
        }
        sync_pattern_updates(shared, &mut sequencer);
        let render_started = Instant::now();
        let report = sequencer.render(back_start, back);
        let render_us = duration_us_u32(Instant::now().saturating_duration_since(render_started));
        publish_render(shared, &report, u64::from(render_us));
        let service_us = duration_us_u32(Instant::now().saturating_duration_since(service_started));
        let observation = AudioServiceObservation {
            service_us,
            render_us,
            dma_cadence_us,
            handoff_us,
            peak_primary_voices: report.peak_primary_voice_count,
        };
        previous_observation = Some(observation);
        pending_metrics = Some(AudioServiceMetrics {
            observation,
            observed_load_level: load_controller.level(),
            observed_policy: policy,
            next_load_level: load_controller.level(),
            next_policy: policy,
            load_ewma_us: load_controller.ewma_us(),
            window_max_us: load_controller.window_max_us(),
        });

        transfer.await;
        previous_handoff_started = Some(Instant::now());

        if sm.tx().stalled() {
            underrun_count = shared.lock(|state| {
                let mut state = state.borrow_mut();
                state.underrun_count = state.underrun_count.saturating_add(1);
                state.underrun_count
            });
        }

        front_start = back_start;
        mem::swap(&mut front, &mut back);
    }
}

fn duration_us_u32(duration: Duration) -> u32 {
    duration.as_micros().min(u64::from(u32::MAX)) as u32
}

fn record_audio_service_metrics(state: &mut SharedState, metrics: AudioServiceMetrics) {
    state.last_render_time_us = metrics.observation.render_us;
    state.last_audio_service_time_us = metrics.observation.service_us;
    state.last_peak_primary_voices = metrics.observation.peak_primary_voices;
    state.max_peak_primary_voices = state
        .max_peak_primary_voices
        .max(metrics.observation.peak_primary_voices);
    if metrics.observation.service_us > state.max_audio_service_time_us {
        state.worst_service_primary_voices = metrics.observation.peak_primary_voices;
        state.worst_service_voice_limit = metrics.observed_policy.max_primary_voices;
        state.worst_service_load_level = metrics.observed_load_level;
    }
    state.max_audio_service_time_us = state
        .max_audio_service_time_us
        .max(metrics.observation.service_us);
    if u64::from(metrics.observation.service_us) > AUDIO_BLOCK_DEADLINE_US {
        state.audio_service_deadline_miss_count =
            state.audio_service_deadline_miss_count.saturating_add(1);
    }
    state.max_dma_cadence_us = state
        .max_dma_cadence_us
        .max(metrics.observation.dma_cadence_us);
    state.max_dma_handoff_us = state.max_dma_handoff_us.max(metrics.observation.handoff_us);
    if state.audio_load_level != metrics.next_load_level {
        state.audio_load_transition_count = state.audio_load_transition_count.saturating_add(1);
    }
    state.audio_load_level = metrics.next_load_level;
    state.effective_voice_limit = metrics.next_policy.max_primary_voices;
    state.min_effective_voice_limit = state
        .min_effective_voice_limit
        .min(metrics.next_policy.max_primary_voices);
    state.audio_load_ewma_us = metrics.load_ewma_us;
    state.audio_load_window_max_us = metrics.window_max_us;
}

fn sync_pattern_updates(shared: &Shared, sequencer: &mut Sequencer<'_>) {
    shared.lock(|state| {
        let mut state = state.borrow_mut();
        let dirty = state.take_pattern_dirty_mask();
        for pad in 0..BEAT_PAD_COUNT {
            if dirty & (1 << pad) != 0
                && let Some(pattern) = state.pattern(pad).copied()
            {
                sequencer.set_pattern(pad, pattern);
            }
        }
    });
}

fn publish_render(shared: &Shared, report: &looptic::RenderReport, render_us: u64) {
    shared.lock(|state| {
        let mut state = state.borrow_mut();
        state.record_sampler_report(report);
        state.max_render_time_us = state
            .max_render_time_us
            .max(render_us.min(u64::from(u32::MAX)) as u32);
        if render_us > AUDIO_BLOCK_DEADLINE_US {
            state.render_deadline_miss_count = state.render_deadline_miss_count.saturating_add(1);
        }
        for (pad, trigger) in report.latest_visual_triggers.iter().enumerate() {
            if let Some(frame) = trigger {
                state.latest_trigger_frames[pad] = *frame;
            }
        }
    });
}

#[embassy_executor::task]
async fn controls_task(
    mut keys: [Input<'static>; KEY_COUNT],
    mut encoder: PioEncoder<'static, PIO1, 1>,
    encoder_button: Input<'static>,
    shared: &'static Shared,
    ui: &'static UiShared,
) {
    let mut debouncer = KeyDebouncer::new(5);
    let mut encoder_button_debouncer = KeyDebouncer::new(5);
    let mut selection = HeldPadSelection::new();
    let mut mute_button = MuteButtonState::new();
    let mut volume_control = VolumeControlState::new();
    let mut next_scan = Instant::now();
    let mut encoder_acceleration = EncoderAcceleration::new();

    loop {
        if let Ok(direction) = with_deadline(next_scan, encoder.read()).await {
            let ui_state = ui.lock(|state| *state.borrow());
            let target = EncoderTarget::for_controls(
                ui_state.selected_pad,
                ui_state.encoder_pressed,
                ui_state.pattern_editor.active,
                ui_state.volume_target,
                ui_state.sample_pressed,
            );
            let direction_delta = match direction {
                Direction::Clockwise => 1,
                Direction::CounterClockwise => -1,
            };
            let choosing_pattern_all = ui_state.pattern_editor.all_menu.is_some();
            let delta = if matches!(target, EncoderTarget::Sample(_)) || choosing_pattern_all {
                direction_delta
            } else {
                encoder_acceleration.update(Instant::now().as_millis(), target, direction_delta)
            };
            match target {
                EncoderTarget::PatternStep(pad) => {
                    let division = shared.lock(|state| state.borrow().desired_beats[pad]);
                    ui.lock(|state| {
                        state.borrow_mut().pattern_editor.scroll(division, delta);
                    });
                }
                EncoderTarget::Sample(Some(pad)) => shared.lock(|state| {
                    let mut state = state.borrow_mut();
                    if let Some(current) = state.pad_sample(pad) {
                        let selected = adjust_sample_selection(current, delta);
                        if state.set_pad_sample(pad, selected)
                            && let Some(preview) = looptic::PreviewRequest::new(pad, selected)
                        {
                            let _ = state.queue_preview(preview);
                        }
                    }
                }),
                EncoderTarget::Sample(None) => {}
                _ => shared.lock(|state| {
                    let mut state = state.borrow_mut();
                    apply_encoder_delta(&mut state, target, delta);
                }),
            }
        }

        let now = Instant::now();
        if now >= next_scan {
            let mut raw_mask = 0_u16;
            for (pad, key) in keys.iter_mut().enumerate() {
                if key.is_low() {
                    raw_mask |= 1 << pad;
                }
            }

            let changes = debouncer.update(raw_mask);
            selection.apply(changes);
            let button_changes =
                encoder_button_debouncer.update(u16::from(encoder_button.is_low()));
            let encoder_pressed = encoder_button_debouncer.stable_mask() & 1 != 0;
            let primary = selection.selected();
            let volume_pressed = debouncer.stable_mask() & (1 << VOLUME_KEY_INDEX) != 0;
            let sample_pressed = debouncer.stable_mask() & (1 << SAMPLE_KEY_INDEX) != 0;
            volume_control.update(volume_pressed, changes, primary);
            let volume_target = volume_control.active_target();
            let mut next_ui = ui.lock(|state| *state.borrow());
            let volume_was_pressed = next_ui.volume_pressed;
            let sample_was_pressed = next_ui.sample_pressed;
            let mute_bit = 1_u16 << MUTE_KEY_INDEX;
            let now_ms = now.as_millis();
            if changes.pressed & mute_bit != 0 {
                let target = MuteTarget::for_selected_pad(primary);
                if mute_button.press(target, now_ms) {
                    shared.lock(|state| {
                        state.borrow_mut().begin_mute_gesture(target);
                    });
                }
            }
            if changes.released & mute_bit != 0
                && let Some(release) = mute_button.release(now_ms)
            {
                shared.lock(|state| {
                    state.borrow_mut().end_mute_gesture(release);
                });
            }
            let division = primary.map_or(0, |pad| {
                shared.lock(|state| state.borrow().desired_beats[pad])
            });
            next_ui.selected_pad = primary;
            next_ui.encoder_pressed = encoder_pressed;
            next_ui.volume_pressed = volume_pressed;
            next_ui.sample_pressed = sample_pressed;
            next_ui.volume_target = volume_target;
            next_ui.pattern_editor.update_primary(primary, division);
            let editor_action = if pattern_button_allowed(
                volume_pressed,
                volume_was_pressed,
                sample_pressed,
                sample_was_pressed,
            ) && button_changes.pressed & 1 != 0
            {
                next_ui.pattern_editor.button_pressed(primary, division)
            } else {
                None
            };
            match editor_action {
                Some(PatternEditorAction::Toggle { pad, step }) => shared.lock(|state| {
                    state.borrow_mut().toggle_pattern_step(pad, step);
                }),
                Some(PatternEditorAction::SetAll { pad, enabled }) => {
                    shared.lock(|state| {
                        state.borrow_mut().set_pattern_all(pad, enabled);
                    });
                    encoder_acceleration = EncoderAcceleration::new();
                }
                Some(
                    PatternEditorAction::AllMenuOpened | PatternEditorAction::AllMenuCancelled,
                ) => {
                    encoder_acceleration = EncoderAcceleration::new();
                }
                Some(PatternEditorAction::Entered) | None => {}
            }
            ui.lock(|state| *state.borrow_mut() = next_ui);

            next_scan += KEY_SCAN_INTERVAL;
            if next_scan <= now {
                // Do not perform a burst of artificial debounce samples after a long stall.
                next_scan = now + KEY_SCAN_INTERVAL;
            }
        }
    }
}

#[embassy_executor::task]
async fn led_task(
    mut pixels: PioWs2812<'static, PIO1, 0, KEY_COUNT, Grb>,
    mut status_led: Output<'static>,
    shared: &'static Shared,
    ui: &'static UiShared,
) {
    loop {
        let ui_state = ui.lock(|state| *state.borrow());
        let mute_target = MuteTarget::for_selected_pad(ui_state.selected_pad);
        let volume_target = ui_state
            .volume_target
            .unwrap_or(VolumeTarget::for_selected_pad(ui_state.selected_pad));
        let (playback_frame, triggers, underruns, brightness, mute_active, volume) =
            shared.lock(|state| {
                let state = state.borrow();
                (
                    state.playback_frame,
                    state.latest_trigger_frames,
                    state.underrun_count,
                    state.led_brightness_percent,
                    state.mute_indicator_active(mute_target).unwrap_or(false),
                    state.volume_percent(volume_target).unwrap_or(0),
                )
            });
        let brightness_preview = ui_state.selected_pad.is_none()
            && ui_state.encoder_pressed
            && !ui_state.volume_pressed
            && !ui_state.sample_pressed;

        let mut colors = [RGB8::default(); KEY_COUNT];
        for pad in 0..BEAT_PAD_COUNT {
            if brightness_preview
                || led_pulse_active(playback_frame, triggers[pad], SAMPLE_RATE / 10)
            {
                let (r, g, b) = scale_color(colorwheel((21 * pad) as u8), brightness);
                colors[pad] = RGB8 { r, g, b };
            }
        }
        let (r, g, b) = mute_led_color(mute_active, brightness);
        colors[MUTE_KEY_INDEX] = RGB8 { r, g, b };
        let (r, g, b) = volume_led_color(volume, brightness);
        colors[VOLUME_KEY_INDEX] = RGB8 { r, g, b };
        let (r, g, b) = sample_led_color(brightness);
        colors[SAMPLE_KEY_INDEX] = RGB8 { r, g, b };
        pixels.write(&colors).await;

        if underruns != 0 || FATAL_FAULT.load(Ordering::Relaxed) {
            status_led.set_high();
        }
        // Schedule from now rather than replaying every missed refresh after
        // an audio-pressure episode. Visual history is already represented by
        // the latest trigger frame and does not benefit from catch-up writes.
        Timer::after(LED_INTERVAL).await;
    }
}

fn draw_pattern_editor<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    shared: &Shared,
    pad: usize,
    cursor: u16,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    const VISIBLE_ROWS: u16 = 5;
    let (division, cursor, window_start, fill_state, enabled_rows) = shared.lock(|state| {
        let state = state.borrow();
        let division = state.desired_beats[pad];
        let cursor = cursor.min(division);
        let window_start = pattern_window_start(cursor, division, VISIBLE_ROWS);
        let mut enabled_rows = [false; VISIBLE_ROWS as usize];
        let mut fill_state = PatternFillState::Empty;
        if let Some(pattern) = state.pattern(pad) {
            fill_state = pattern.fill_state();
            for row in 0..VISIBLE_ROWS {
                let entry = window_start + row;
                if entry != 0 && entry <= division {
                    let step = entry - 1;
                    enabled_rows[usize::from(row)] =
                        pattern.step_enabled(step, division).unwrap_or(false);
                }
            }
        }
        (division, cursor, window_start, fill_state, enabled_rows)
    });

    let mut header: String<24> = String::new();
    let _ = write!(&mut header, "LoopTic P{} {}", pad + 1, division);
    let _ = Text::with_baseline(&header, Point::new(0, 0), style, Baseline::Top).draw(display);

    for row in 0..VISIBLE_ROWS {
        let entry = window_start + row;
        if entry > division {
            break;
        }
        let marker = if entry == cursor { '>' } else { ' ' };
        let mut line: String<16> = String::new();
        if entry == 0 {
            let state = match fill_state {
                PatternFillState::Empty => "off",
                PatternFillState::Full => "ON",
                PatternFillState::Mixed => "mix",
            };
            let _ = write!(&mut line, "{} All {}", marker, state);
        } else {
            let state = if enabled_rows[usize::from(row)] {
                "ON"
            } else {
                "off"
            };
            let _ = write!(&mut line, "{} {:04} {}", marker, entry, state);
        }
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 14 + i32::from(row) * 10),
            style,
            Baseline::Top,
        )
        .draw(display);
    }

    if division == 0 {
        let _ = Text::with_baseline("No triggers", Point::new(0, 28), style, Baseline::Top)
            .draw(display);
    }
}

fn draw_pattern_all_menu<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    pad: usize,
    selected: PatternAllChoice,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let mut header: String<24> = String::new();
    let _ = write!(&mut header, "LoopTic P{} All", pad + 1);
    let _ = Text::with_baseline(&header, Point::new(0, 0), style, Baseline::Top).draw(display);

    for (row, (choice, label)) in [
        (PatternAllChoice::Cancel, "Cancel"),
        (PatternAllChoice::All, "All"),
        (PatternAllChoice::None, "None"),
    ]
    .iter()
    .enumerate()
    {
        let marker = if *choice == selected { '>' } else { ' ' };
        let mut line: String<16> = String::new();
        let _ = write!(&mut line, "{} {}", marker, label);
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 16 + row as i32 * 14),
            style,
            Baseline::Top,
        )
        .draw(display);
    }
}

fn draw_audio_diagnostics<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    metrics: AudioDisplayMetrics,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let level = match metrics.level {
        LoadLevel::Normal => 'N',
        LoadLevel::Pressure => 'P',
        LoadLevel::Emergency => 'E',
        LoadLevel::RecoveryDither | LoadLevel::RecoveryTails | LoadLevel::RecoveryStarts => 'R',
    };
    let mut line: String<24> = String::new();
    let _ = write!(
        &mut line,
        "Load {} V{}/{}",
        level, metrics.active_voices, metrics.voice_limit
    );
    let _ = Text::with_baseline(&line, Point::new(0, 0), style, Baseline::Top).draw(display);

    line.clear();
    let _ = write!(
        &mut line,
        "Svc {}/{} us",
        metrics.last_service_us, metrics.max_service_us
    );
    let _ = Text::with_baseline(&line, Point::new(0, 12), style, Baseline::Top).draw(display);

    line.clear();
    let _ = write!(
        &mut line,
        "Ren {}/{} us",
        metrics.last_render_us, metrics.max_render_us
    );
    let _ = Text::with_baseline(&line, Point::new(0, 24), style, Baseline::Top).draw(display);

    line.clear();
    let _ = write!(&mut line, "DMA {} us", metrics.max_dma_cadence_us);
    let _ = Text::with_baseline(&line, Point::new(0, 36), style, Baseline::Top).draw(display);

    line.clear();
    let _ = write!(
        &mut line,
        "Late {} U {}",
        metrics.deadline_misses, metrics.underruns
    );
    let _ = Text::with_baseline(&line, Point::new(0, 48), style, Baseline::Top).draw(display);
}

#[embassy_executor::task]
async fn display_task(resources: OledResources, shared: &'static Shared, ui: &'static UiShared) {
    let mut spi_config = spi::Config::default();
    spi_config.frequency = 10_000_000;
    let spi = Spi::new_blocking_txonly(resources.spi, resources.sck, resources.mosi, spi_config);
    let dc = Output::new(resources.dc, Level::Low);
    let cs = Output::new(resources.cs, Level::High);
    let mut reset = Output::new(resources.reset, Level::High);
    let mut delay = Delay;
    let mut display: GraphicsMode<_> = Builder::new().connect_spi(spi, dc, cs).into();

    if display.reset(&mut reset, &mut delay).is_err() || display.init().is_err() {
        FATAL_FAULT.store(true, Ordering::Relaxed);
        loop {
            Timer::after_secs(1).await;
        }
    }

    let mut last_value: Option<DisplayStateKey> = None;
    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    loop {
        let ui_state = ui.lock(|state| *state.borrow());
        let target = EncoderTarget::for_controls(
            ui_state.selected_pad,
            ui_state.encoder_pressed,
            ui_state.pattern_editor.active,
            ui_state.volume_target,
            ui_state.sample_pressed,
        );
        let diagnostics_active = ui_state.selected_pad.is_none()
            && ui_state.sample_pressed
            && ui_state.encoder_pressed
            && !ui_state.volume_pressed;
        let (displayed_value, pattern_revision, diagnostics) = shared.lock(|state| {
            let state = state.borrow();
            let value = match target {
                EncoderTarget::BaseInterval => (state.base_interval_ms, 0),
                EncoderTarget::LedBrightness => (u32::from(state.led_brightness_percent), 0),
                EncoderTarget::Pad(pad) => (u32::from(state.desired_beats[pad]), 0),
                EncoderTarget::PatternStep(_) => (
                    u32::from(ui_state.pattern_editor.cursor),
                    state.pattern_revision,
                ),
                EncoderTarget::Volume(target) => {
                    (u32::from(state.volume_percent(target).unwrap_or(0)), 0)
                }
                EncoderTarget::Sample(Some(pad)) => (
                    state
                        .pad_sample(pad)
                        .map_or(0, |sample| sample.index() as u32),
                    0,
                ),
                EncoderTarget::Sample(None) => (0, 0),
            };
            let diagnostics = diagnostics_active.then_some(AudioDisplayMetrics {
                level: state.audio_load_level,
                active_voices: state.last_peak_primary_voices,
                voice_limit: state.effective_voice_limit,
                last_service_us: state.last_audio_service_time_us,
                max_service_us: state.max_audio_service_time_us,
                last_render_us: state.last_render_time_us,
                max_render_us: state.max_render_time_us,
                max_dma_cadence_us: state.max_dma_cadence_us,
                deadline_misses: state.audio_service_deadline_miss_count,
                underruns: state.underrun_count,
            });
            (value.0, value.1, diagnostics)
        });
        let pattern_all_choice = ui_state.pattern_editor.all_menu.map(|menu| menu.choice);
        let value = DisplayStateKey {
            target,
            displayed_value,
            pattern_revision,
            pattern_all_choice,
            diagnostics,
        };

        if last_value != Some(value) {
            display.clear();
            if let Some(metrics) = diagnostics {
                draw_audio_diagnostics(&mut display, style, metrics);
            } else {
                match target {
                    EncoderTarget::Pad(_) => {
                        let _ =
                            Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                                .draw(&mut display);
                        let mut line: String<24> = String::new();
                        let _ = write!(&mut line, "Beat {}", displayed_value);
                        let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                            .draw(&mut display);
                    }
                    EncoderTarget::BaseInterval => {
                        let _ =
                            Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                                .draw(&mut display);
                        let mut line: String<24> = String::new();
                        let _ = write!(&mut line, "Base {} ms", displayed_value);
                        let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                            .draw(&mut display);
                    }
                    EncoderTarget::LedBrightness => {
                        let _ =
                            Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                                .draw(&mut display);
                        let mut line: String<24> = String::new();
                        let _ = write!(&mut line, "Light {}%", displayed_value);
                        let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                            .draw(&mut display);
                    }
                    EncoderTarget::PatternStep(pad) => {
                        if let Some(menu) = ui_state.pattern_editor.all_menu {
                            draw_pattern_all_menu(&mut display, style, pad, menu.choice);
                        } else {
                            draw_pattern_editor(
                                &mut display,
                                style,
                                shared,
                                pad,
                                ui_state.pattern_editor.cursor,
                            );
                        }
                    }
                    EncoderTarget::Volume(VolumeTarget::Global) => {
                        let _ =
                            Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                                .draw(&mut display);
                        let mut line: String<24> = String::new();
                        let _ = write!(&mut line, "Master {}%", displayed_value);
                        let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                            .draw(&mut display);
                    }
                    EncoderTarget::Volume(VolumeTarget::Pad(pad)) => {
                        let _ =
                            Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                                .draw(&mut display);
                        let mut line: String<24> = String::new();
                        let _ = write!(&mut line, "P{} Vol {}%", pad + 1, displayed_value);
                        let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                            .draw(&mut display);
                    }
                    EncoderTarget::Sample(Some(pad)) => {
                        let _ =
                            Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                                .draw(&mut display);
                        let sample = displayed_value as usize;
                        let mut line: String<24> = String::new();
                        let name = sample_assets::SAMPLE_NAMES
                            .get(sample)
                            .copied()
                            .unwrap_or("?");
                        let _ = write!(
                            &mut line,
                            "P{} Sample {:02}/{}",
                            pad + 1,
                            sample + 1,
                            SAMPLE_COUNT
                        );
                        let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                            .draw(&mut display);
                        let _ = Text::with_baseline(name, Point::new(0, 30), style, Baseline::Top)
                            .draw(&mut display);
                    }
                    EncoderTarget::Sample(None) => {
                        let _ =
                            Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                                .draw(&mut display);
                        let _ =
                            Text::with_baseline("Sample", Point::new(0, 16), style, Baseline::Top)
                                .draw(&mut display);
                        let _ = Text::with_baseline(
                            "Hold beat",
                            Point::new(0, 30),
                            style,
                            Baseline::Top,
                        )
                        .draw(&mut display);
                    }
                }
            }

            if display.flush().is_err() {
                FATAL_FAULT.store(true, Ordering::Relaxed);
            }
            last_value = Some(value);
        }

        // A stale OLED refresh is disposable; never build a catch-up backlog
        // that could keep controls latent after audio load has recovered.
        Timer::after(DISPLAY_INTERVAL).await;
    }
}

#[interrupt]
unsafe fn SWI_IRQ_1() {
    unsafe { EXECUTOR_HIGH.on_interrupt() }
}

#[entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());
    let shared = SHARED.init(Mutex::new(RefCell::new(SharedState::default())));
    let ui = UI_SHARED.init(Mutex::new(RefCell::new(UiState::default())));

    // The built-in amplifier must remain disabled while the external line output is used.
    let _speaker_enable = Output::new(p.PIN_14, Level::Low);
    let mut status_led = Output::new(p.PIN_13, Level::Low);

    let catalog = match sample_assets::parse_catalog() {
        Ok(catalog) => catalog,
        Err(_) => {
            // A steady low level is silent through the SynthPlug's AC-coupled output.
            let silent_audio = Output::new(p.PIN_20, Level::Low);
            fatal(&mut status_led, silent_audio)
        }
    };
    let sequencer = Sequencer::new(catalog);

    let audio_sm = configure_audio_pio(p.PIO0, p.PIN_20);

    interrupt::SWI_IRQ_1.set_priority(Priority::P2);
    let high_spawner = EXECUTOR_HIGH.start(interrupt::SWI_IRQ_1);
    high_spawner.spawn(audio_task(audio_sm, p.DMA_CH0, sequencer, shared).unwrap());

    let Pio {
        mut common,
        sm0,
        sm1,
        ..
    } = Pio::new(p.PIO1, Irqs);
    let pixel_program = PioWs2812Program::new(&mut common);
    let pixels = PioWs2812::new(&mut common, sm0, p.DMA_CH1, Irqs, p.PIN_19, &pixel_program);
    let encoder_program = PioEncoderProgram::new(&mut common);
    let encoder = PioEncoder::new(&mut common, sm1, p.PIN_17, p.PIN_18, &encoder_program);
    let encoder_button = Input::new(p.PIN_0, Pull::Up);

    let keys = [
        Input::new(p.PIN_1, Pull::Up),
        Input::new(p.PIN_2, Pull::Up),
        Input::new(p.PIN_3, Pull::Up),
        Input::new(p.PIN_4, Pull::Up),
        Input::new(p.PIN_5, Pull::Up),
        Input::new(p.PIN_6, Pull::Up),
        Input::new(p.PIN_7, Pull::Up),
        Input::new(p.PIN_8, Pull::Up),
        Input::new(p.PIN_9, Pull::Up),
        Input::new(p.PIN_10, Pull::Up),
        Input::new(p.PIN_11, Pull::Up),
        Input::new(p.PIN_12, Pull::Up),
    ];

    let low_executor = EXECUTOR_LOW.init(Executor::new());
    low_executor.run(|spawner| {
        spawner.spawn(controls_task(keys, encoder, encoder_button, shared, ui).unwrap());
        spawner.spawn(led_task(pixels, status_led, shared, ui).unwrap());
        let oled = OledResources {
            spi: p.SPI1,
            sck: p.PIN_26,
            mosi: p.PIN_27,
            cs: p.PIN_22,
            reset: p.PIN_23,
            dc: p.PIN_24,
        };
        spawner.spawn(display_task(oled, shared, ui).unwrap());
    })
}

fn fatal(status_led: &mut Output<'static>, _silent_audio: Output<'static>) -> ! {
    status_led.set_high();
    loop {
        cortex_m::asm::wfi();
    }
}
