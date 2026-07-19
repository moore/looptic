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
use embassy_sync::signal::Signal;
use embassy_time::{Delay, Duration, Instant, Timer, with_deadline};
use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_6X10};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Triangle};
use embedded_graphics::text::{Baseline, Text};
use fixed::types::U24F8;
use heapless::String;
use looptic::load_control::{AudioLoadController, LoadLevel, RenderPolicy};
use looptic::{
    AUDIO_BLOCK_FRAMES, BEAT_PAD_COUNT, KEY_COUNT, KeyDebouncer, MUTE_KEY_INDEX, MuteButtonState,
    MuteScanAction, MuteTarget, PatternAllChoice, PatternEditorAction, PatternFillState,
    PatternVolumeTarget, RETURN_KEY_INDEX, ResetAllChoice, RootMode, SAMPLE_COUNT, SAMPLE_RATE,
    SILENCE_PWM_WORD, SONG_ENCODED_MAX_LEN, SONG_FORMAT_VERSION, Sequencer, SharedState,
    SongBrowserPurpose, SongConfirmChoice, SongLibraryStatus, SongMenuOperation, SongSlot,
    SongSlotOccupancy, SongStorageOperation, SongUiStatus, SongsView, StoredSongV3, UiAction,
    UiController, UiDisplayModel, UiEncoderAcceleration, UiEncoderTarget, UiPage, VOLUME_KEY_INDEX,
    VolumeTarget, adjust_base_interval, adjust_beat_multiplier, adjust_led_brightness,
    adjust_pad_cycle_length, adjust_sample_selection, decode_song, encode_song_v3,
    led_pulse_active, mute_led_color, resolve_mute_scan, return_led_color, sample_assets,
    sample_selection_preview_request, scroll_menu_window, voice_led_color, volume_led_color,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct UiState {
    controller: UiController,
    volume_pressed: bool,
    song_library: SongLibraryStatus,
    pending_song_operation: Option<SongStorageOperation>,
    clean_song_revision: u32,
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
struct DisplayStateKey {
    model: UiDisplayModel,
    displayed_value: u32,
    pattern_revision: u32,
}

static SHARED: StaticCell<Shared> = StaticCell::new();
static UI_SHARED: StaticCell<UiShared> = StaticCell::new();
static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
static EXECUTOR_LOW: StaticCell<Executor> = StaticCell::new();
static AUDIO_DMA_FRONT: StaticCell<[u32; AUDIO_BLOCK_FRAMES]> = StaticCell::new();
static AUDIO_DMA_BACK: StaticCell<[u32; AUDIO_BLOCK_FRAMES]> = StaticCell::new();
static SONG_WORK_BUFFER: StaticCell<[u8; 4096]> = StaticCell::new();
static SONG_RECORD_BUFFER: StaticCell<[u8; SONG_ENCODED_MAX_LEN]> = StaticCell::new();
static FATAL_FAULT: AtomicBool = AtomicBool::new(false);
static STORAGE_BOOT_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static AUDIO_PAUSE_REQUESTED: AtomicBool = AtomicBool::new(false);
static AUDIO_PAUSED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static AUDIO_RESUME: Signal<CriticalSectionRawMutex, ()> = Signal::new();

const AUDIO_CLOCK_DIVIDER_BITS: u32 = 667;
const KEY_SCAN_INTERVAL: Duration = Duration::from_millis(1);
const DISPLAY_INTERVAL: Duration = Duration::from_millis(34);
const LED_INTERVAL: Duration = Duration::from_millis(5);
const STORAGE_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(5);
const AUDIO_BLOCK_DEADLINE_US: u64 = looptic::load_control::AUDIO_BLOCK_BUDGET_US as u64;

enum SongStorageBackend {
    Blank(looptic::flash_storage::BlankSongStorage),
    Ready(&'static mut looptic::flash_storage::SongMap),
    Unsupported(looptic::flash_storage::UnsupportedSongStorage),
    Corrupt,
    Unavailable,
}

enum SongLoadResult {
    Loaded {
        revision: u32,
        divisions: [u16; BEAT_PAD_COUNT],
    },
    Empty,
    Unsupported {
        found: u16,
        supported: u16,
    },
    Corrupt,
    Failed,
}

fn unsupported_storage_versions(info: looptic::flash_storage::SuperblockInfo) -> (u32, u32) {
    if info.header_version != looptic::flash_storage::SUPERBLOCK_HEADER_VERSION {
        (
            u32::from(info.header_version),
            u32::from(looptic::flash_storage::SUPERBLOCK_HEADER_VERSION),
        )
    } else if info.storage_layout_version != looptic::flash_storage::STORAGE_LAYOUT_VERSION {
        (
            info.storage_layout_version,
            looptic::flash_storage::STORAGE_LAYOUT_VERSION,
        )
    } else {
        (
            u32::from(info.sequential_storage_version),
            u32::from(looptic::flash_storage::SEQUENTIAL_STORAGE_FORMAT_VERSION),
        )
    }
}

fn unavailable_storage_status(backend: &SongStorageBackend) -> Option<SongUiStatus> {
    match backend {
        SongStorageBackend::Unsupported(storage) => {
            let (found, supported) = unsupported_storage_versions(storage.info());
            Some(SongUiStatus::UnsupportedStorage { found, supported })
        }
        SongStorageBackend::Corrupt => Some(SongUiStatus::Corrupt { slot: None }),
        SongStorageBackend::Unavailable => Some(SongUiStatus::Unavailable),
        SongStorageBackend::Blank(_) | SongStorageBackend::Ready(_) => None,
    }
}

fn set_song_status(ui: &UiShared, status: SongUiStatus) {
    ui.lock(|state| state.borrow_mut().controller.set_song_status(status));
}

async fn pause_audio_for_storage() {
    AUDIO_PAUSE_REQUESTED.store(true, Ordering::Release);
    AUDIO_PAUSED.wait().await;
}

fn resume_audio_after_storage() {
    AUDIO_PAUSE_REQUESTED.store(false, Ordering::Release);
    AUDIO_RESUME.signal(());
}

#[embassy_executor::task]
async fn storage_task(
    flash: Peri<'static, peripherals::FLASH>,
    shared: &'static Shared,
    ui: &'static UiShared,
) {
    let work_buffer = SONG_WORK_BUFFER.init([0_u8; 4096]);
    let record_buffer = SONG_RECORD_BUFFER.init([0_u8; SONG_ENCODED_MAX_LEN]);

    let mut boot_status = None;
    let mut backend = match looptic::flash_storage::probe(flash) {
        Ok(looptic::flash_storage::StorageProbe::Blank(blank)) => SongStorageBackend::Blank(blank),
        Ok(looptic::flash_storage::StorageProbe::Incomplete(blank)) => {
            SongStorageBackend::Blank(blank)
        }
        Ok(looptic::flash_storage::StorageProbe::Ready(map)) => SongStorageBackend::Ready(map),
        Ok(looptic::flash_storage::StorageProbe::Unsupported(storage)) => {
            let (found, supported) = unsupported_storage_versions(storage.info());
            boot_status = Some(SongUiStatus::UnsupportedStorage { found, supported });
            SongStorageBackend::Unsupported(storage)
        }
        Ok(looptic::flash_storage::StorageProbe::Corrupt(_)) => {
            boot_status = Some(SongUiStatus::Corrupt { slot: None });
            SongStorageBackend::Corrupt
        }
        Err(_) => {
            boot_status = Some(SongUiStatus::Unavailable);
            SongStorageBackend::Unavailable
        }
    };

    let scanned = match &mut backend {
        SongStorageBackend::Ready(map) => map.scan_occupancy(work_buffer).await.ok(),
        _ => Some([0_u32; looptic::SONG_SLOT_COUNT / 32]),
    };
    let occupancy = match scanned {
        Some(words) => SongSlotOccupancy::from_words(words),
        None => {
            backend = SongStorageBackend::Unavailable;
            boot_status = Some(SongUiStatus::Unavailable);
            SongSlotOccupancy::empty()
        }
    };
    ui.lock(|state| {
        let mut state = state.borrow_mut();
        state.song_library.occupied = occupancy;
        if let Some(status) = boot_status {
            state.controller.set_song_status(status);
        }
    });
    // Audio startup waits until the partition has been classified and the
    // one-pass occupancy index is complete. A healthy scan is read-only, but
    // the supported journal may repair an interrupted operation here.
    STORAGE_BOOT_READY.signal(());

    loop {
        let operation = ui.lock(|state| state.borrow_mut().pending_song_operation.take());
        let Some(operation) = operation else {
            Timer::after(STORAGE_COMMAND_POLL_INTERVAL).await;
            continue;
        };

        let library = ui.lock(|state| state.borrow().song_library);
        let live_revision = shared.lock(|state| state.borrow().song_revision);

        if operation == SongStorageOperation::SaveCurrent && library.current_slot.is_none() {
            ui.lock(|state| {
                state.borrow_mut().controller.open_save_as(None);
            });
            continue;
        }
        if operation != SongStorageOperation::Format
            && let Some(status) = unavailable_storage_status(&backend)
        {
            set_song_status(ui, status);
            continue;
        }

        match operation {
            SongStorageOperation::Format => {
                let unsupported = match mem::replace(&mut backend, SongStorageBackend::Unavailable)
                {
                    SongStorageBackend::Unsupported(storage) => storage,
                    replacement => {
                        backend = replacement;
                        set_song_status(ui, SongUiStatus::Failed { operation });
                        continue;
                    }
                };
                pause_audio_for_storage().await;
                backend = match unsupported
                    .reformat(SONG_FORMAT_VERSION, |percent| {
                        set_song_status(ui, SongUiStatus::Formatting { percent });
                    })
                    .await
                {
                    Ok(map) => SongStorageBackend::Ready(map),
                    Err(_) => SongStorageBackend::Unavailable,
                };
                resume_audio_after_storage();
                if matches!(backend, SongStorageBackend::Ready(_)) {
                    ui.lock(|state| {
                        let mut state = state.borrow_mut();
                        state.song_library = SongLibraryStatus::empty();
                        state.clean_song_revision = 0;
                        state
                            .controller
                            .set_song_status(SongUiStatus::Success { operation });
                    });
                } else {
                    set_song_status(ui, SongUiStatus::Failed { operation });
                }
            }
            SongStorageOperation::SaveCurrent | SongStorageOperation::SaveAs { .. } => {
                let slot = match operation {
                    SongStorageOperation::SaveCurrent => library
                        .current_slot
                        .expect("SaveCurrent without a slot was redirected"),
                    SongStorageOperation::SaveAs { slot } => slot,
                    _ => unreachable!(),
                };
                if operation == SongStorageOperation::SaveCurrent
                    && live_revision == ui.lock(|state| state.borrow().clean_song_revision)
                {
                    set_song_status(ui, SongUiStatus::NoChanges { slot });
                    continue;
                }

                pause_audio_for_storage().await;
                let song = shared.lock(|state| StoredSongV3::snapshot(&state.borrow()));
                let encoded_len = match encode_song_v3(&song, record_buffer) {
                    Ok(encoded) => encoded.len(),
                    Err(_) => {
                        resume_audio_after_storage();
                        set_song_status(ui, SongUiStatus::Failed { operation });
                        continue;
                    }
                };

                if matches!(backend, SongStorageBackend::Blank(_)) {
                    let blank = match mem::replace(&mut backend, SongStorageBackend::Unavailable) {
                        SongStorageBackend::Blank(blank) => blank,
                        _ => unreachable!(),
                    };
                    backend = match blank.initialize(SONG_FORMAT_VERSION).await {
                        Ok(map) => SongStorageBackend::Ready(map),
                        Err(_) => SongStorageBackend::Unavailable,
                    };
                }
                let saved = match &mut backend {
                    SongStorageBackend::Ready(map) => map
                        .save(
                            slot.storage_key(),
                            work_buffer,
                            &record_buffer[..encoded_len],
                        )
                        .await
                        .is_ok(),
                    _ => false,
                };
                resume_audio_after_storage();

                if saved {
                    ui.lock(|state| {
                        let mut state = state.borrow_mut();
                        state.song_library.occupied.set(slot, true);
                        state.song_library.current_slot = Some(slot);
                        state.song_library.dirty = false;
                        state.clean_song_revision = live_revision;
                        state
                            .controller
                            .set_song_status(SongUiStatus::Success { operation });
                    });
                } else {
                    set_song_status(ui, SongUiStatus::Failed { operation });
                }
            }
            SongStorageOperation::Load { slot } => {
                if !library.occupied.contains(slot) {
                    set_song_status(ui, SongUiStatus::Empty { slot });
                    continue;
                }

                pause_audio_for_storage().await;
                let loaded = match &mut backend {
                    SongStorageBackend::Ready(map) => {
                        match map.load(slot.storage_key(), work_buffer).await {
                            Ok(Some(bytes)) => match decode_song(bytes) {
                                Ok(song) => shared.lock(|state| {
                                    let mut state = state.borrow_mut();
                                    if song.apply_to(&mut state).is_ok() {
                                        SongLoadResult::Loaded {
                                            revision: state.song_revision,
                                            divisions: core::array::from_fn(|pad| {
                                                state
                                                    .effective_pattern_steps(pad)
                                                    .unwrap_or(0)
                                                    .saturating_add(1)
                                            }),
                                        }
                                    } else {
                                        SongLoadResult::Corrupt
                                    }
                                }),
                                Err(looptic::SongDecodeError::UnsupportedVersion {
                                    found,
                                    supported,
                                }) => SongLoadResult::Unsupported { found, supported },
                                Err(_) => SongLoadResult::Corrupt,
                            },
                            Ok(None) => SongLoadResult::Empty,
                            Err(_) => SongLoadResult::Failed,
                        }
                    }
                    _ => SongLoadResult::Failed,
                };
                resume_audio_after_storage();

                match loaded {
                    SongLoadResult::Loaded {
                        revision,
                        divisions,
                    } => ui.lock(|state| {
                        let mut state = state.borrow_mut();
                        state.controller.clamp_pattern_cursors(&divisions);
                        state.song_library.current_slot = Some(slot);
                        state.song_library.dirty = false;
                        state.clean_song_revision = revision;
                        state
                            .controller
                            .set_song_status(SongUiStatus::Success { operation });
                    }),
                    SongLoadResult::Unsupported { found, supported } => set_song_status(
                        ui,
                        SongUiStatus::UnsupportedVersion {
                            slot: Some(slot),
                            found,
                            supported,
                        },
                    ),
                    SongLoadResult::Empty => {
                        ui.lock(|state| {
                            let mut state = state.borrow_mut();
                            state.song_library.occupied.set(slot, false);
                            state
                                .controller
                                .set_song_status(SongUiStatus::Empty { slot });
                        });
                    }
                    SongLoadResult::Corrupt => {
                        set_song_status(ui, SongUiStatus::Corrupt { slot: Some(slot) });
                    }
                    SongLoadResult::Failed => {
                        set_song_status(ui, SongUiStatus::Failed { operation });
                    }
                }
            }
            SongStorageOperation::Copy {
                source,
                destination,
            } => {
                if !library.occupied.contains(source) {
                    set_song_status(ui, SongUiStatus::Empty { slot: source });
                    continue;
                }
                if source == destination {
                    set_song_status(ui, SongUiStatus::NoChanges { slot: source });
                    continue;
                }

                pause_audio_for_storage().await;
                let source_len = match &mut backend {
                    SongStorageBackend::Ready(map) => {
                        match map.load(source.storage_key(), work_buffer).await {
                            Ok(Some(bytes)) if bytes.len() <= record_buffer.len() => {
                                let len = bytes.len();
                                record_buffer[..len].copy_from_slice(bytes);
                                Some(len)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let copied = if let (Some(source_len), SongStorageBackend::Ready(map)) =
                    (source_len, &mut backend)
                {
                    map.save(
                        destination.storage_key(),
                        work_buffer,
                        &record_buffer[..source_len],
                    )
                    .await
                    .is_ok()
                } else {
                    false
                };
                resume_audio_after_storage();

                if copied {
                    if library.current_slot == Some(destination) {
                        // The live controls did not change, but their backing
                        // slot now contains the copied source. Advance the edit
                        // generation so root Save correctly offers to restore
                        // the live song instead of reporting No changes.
                        shared.lock(|state| state.borrow_mut().mark_song_changed());
                    }
                    ui.lock(|state| {
                        let mut state = state.borrow_mut();
                        state.song_library.occupied.set(destination, true);
                        if state.song_library.current_slot == Some(destination) {
                            state.song_library.dirty = true;
                        }
                        state
                            .controller
                            .set_song_status(SongUiStatus::Success { operation });
                    });
                } else {
                    set_song_status(ui, SongUiStatus::Failed { operation });
                }
            }
            SongStorageOperation::Delete { slot } => {
                if !library.occupied.contains(slot) {
                    set_song_status(ui, SongUiStatus::Empty { slot });
                    continue;
                }

                pause_audio_for_storage().await;
                let deleted = match &mut backend {
                    SongStorageBackend::Ready(map) => {
                        map.delete(slot.storage_key(), work_buffer).await.is_ok()
                    }
                    _ => false,
                };
                resume_audio_after_storage();

                if deleted {
                    ui.lock(|state| {
                        let mut state = state.borrow_mut();
                        state.song_library.occupied.set(slot, false);
                        if state.song_library.current_slot == Some(slot) {
                            state.song_library.current_slot = None;
                        }
                        state
                            .controller
                            .set_song_status(SongUiStatus::Success { operation });
                    });
                } else {
                    set_song_status(ui, SongUiStatus::Failed { operation });
                }
            }
        }
    }
}

/// Apply an encoder-driven Pattern editor action. Returns whether changing the
/// Pattern submode should reset encoder acceleration.
fn apply_pattern_editor_action(shared: &Shared, action: PatternEditorAction) -> bool {
    match action {
        PatternEditorAction::Toggle { pad, step } => {
            shared.lock(|state| {
                state.borrow_mut().toggle_pattern_step(pad, step);
            });
            false
        }
        PatternEditorAction::SetAll { pad, enabled } => {
            shared.lock(|state| {
                state.borrow_mut().set_pattern_all(pad, enabled);
            });
            true
        }
        PatternEditorAction::RepeatEditorOpened
        | PatternEditorAction::RepeatEditorClosed
        | PatternEditorAction::AllMenuOpened
        | PatternEditorAction::AllMenuCancelled => true,
    }
}

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
    // Boot-time flash inspection and the occupancy scan happen before PIO is
    // enabled, so even journal recovery cannot contend with an audio deadline.
    STORAGE_BOOT_READY.wait().await;

    let mut dma = dma::Channel::new(dma_peripheral, Irqs);
    let mut front = AUDIO_DMA_FRONT.init([SILENCE_PWM_WORD; AUDIO_BLOCK_FRAMES]);
    let mut back = AUDIO_DMA_BACK.init([SILENCE_PWM_WORD; AUDIO_BLOCK_FRAMES]);

    let mut front_start = 0_u64;
    let (
        initial_beats,
        initial_repeats,
        initial_cycle_lengths,
        initial_pad_samples,
        initial_base_interval,
        initial_mute_mask,
        initial_global_volume,
        initial_pad_volumes,
    ) = shared.lock(|state| {
        let state = state.borrow();
        (
            state.desired_beats,
            *state.pattern_repeats(),
            state.effective_cycle_lengths_ms(),
            *state.pad_samples(),
            state.base_interval_ms,
            state.effective_mute_mask(),
            state.global_volume_percent(),
            *state.pad_volume_percents(),
        )
    });
    sequencer.apply_timing_with_cycles(
        &initial_beats,
        &initial_repeats,
        &initial_cycle_lengths,
        initial_base_interval,
        front_start,
    );
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
        let (
            beats,
            repeats,
            cycle_lengths,
            pad_samples,
            preview,
            base_interval,
            mute_mask,
            global_volume,
            pad_volumes,
            release_all,
        ) = shared.lock(|state| {
            let mut state = state.borrow_mut();
            if let Some(metrics) = pending_metrics.take() {
                record_audio_service_metrics(&mut state, metrics);
            }
            state.playback_frame = front_start;
            (
                state.desired_beats,
                *state.pattern_repeats(),
                state.effective_cycle_lengths_ms(),
                *state.pad_samples(),
                state.take_preview(),
                state.base_interval_ms,
                state.effective_mute_mask(),
                state.global_volume_percent(),
                *state.pad_volume_percents(),
                state.take_release_all_request(),
            )
        });
        sequencer.apply_timing_with_cycles(
            &beats,
            &repeats,
            &cycle_lengths,
            base_interval,
            back_start,
        );
        sequencer.set_pad_samples(&pad_samples);
        sequencer.set_mute_mask(mute_mask);
        sequencer.set_volumes(global_volume, &pad_volumes);
        if release_all {
            sequencer.release_all_voices();
        }
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
            underrun_count = record_audio_underrun(shared);
        }

        front_start = back_start;
        mem::swap(&mut front, &mut back);

        if AUDIO_PAUSE_REQUESTED.load(Ordering::Acquire) {
            // `front` is the already-rendered block immediately following the
            // completed DMA. Let it play, render one final muted release block
            // from the contiguous sequencer state, and only then stop PIO.
            let normal_transfer = sm.tx().dma_push(&mut dma, front, false);
            let fade_start = front_start.wrapping_add(AUDIO_BLOCK_FRAMES as u64);
            sequencer.set_mute_mask(looptic::BEAT_PAD_MASK);
            sequencer.release_all_voices();
            let fade_report = sequencer.render(fade_start, back);
            publish_render(shared, &fade_report, 0);

            normal_transfer.await;
            if sm.tx().stalled() {
                underrun_count = record_audio_underrun(shared);
            }
            let fade_transfer = sm.tx().dma_push(&mut dma, back, false);
            fade_transfer.await;
            if sm.tx().stalled() {
                underrun_count = record_audio_underrun(shared);
            }

            // DMA completion can leave the joined FIFO eight frames ahead.
            // Let centered/faded output drain before freezing the pin level.
            Timer::after_micros(500).await;
            sm.set_enable(false);
            sm.clear_fifos();
            front_start = fade_start.wrapping_add(AUDIO_BLOCK_FRAMES as u64);
            shared.lock(|state| state.borrow_mut().playback_frame = front_start);
            AUDIO_PAUSED.signal(());
            AUDIO_RESUME.wait().await;

            let (
                beats,
                repeats,
                cycle_lengths,
                pad_samples,
                preview,
                base_interval,
                mute_mask,
                global_volume,
                pad_volumes,
                release_all,
            ) = shared.lock(|state| {
                let mut state = state.borrow_mut();
                state.playback_frame = front_start;
                (
                    state.desired_beats,
                    *state.pattern_repeats(),
                    state.effective_cycle_lengths_ms(),
                    *state.pad_samples(),
                    state.take_preview(),
                    state.base_interval_ms,
                    state.effective_mute_mask(),
                    state.global_volume_percent(),
                    *state.pad_volume_percents(),
                    state.take_release_all_request(),
                )
            });
            sequencer.apply_timing_with_cycles(
                &beats,
                &repeats,
                &cycle_lengths,
                base_interval,
                front_start,
            );
            sequencer.set_pad_samples(&pad_samples);
            sequencer.set_mute_mask(mute_mask);
            sequencer.set_volumes(global_volume, &pad_volumes);
            if release_all {
                sequencer.release_all_voices();
            }
            if let Some(preview) = preview {
                let _ = sequencer.queue_preview(preview);
            }
            sync_pattern_updates(shared, &mut sequencer);
            let report = sequencer.render(front_start, front);
            publish_render(shared, &report, 0);

            sm.restart();
            sm.clear_fifos();
            for _ in 0..8 {
                sm.tx().push(SILENCE_PWM_WORD);
            }
            let _ = sm.tx().stalled();
            sm.set_enable(true);

            // The flash pause is not audio workload. Discard cadence samples
            // spanning it so adaptive quality does not treat an explicit Save
            // as CPU overload.
            previous_observation = None;
            pending_metrics = None;
            previous_dma_started = None;
            previous_handoff_started = None;
        }
    }
}

fn record_audio_underrun(shared: &Shared) -> u32 {
    shared.lock(|state| {
        let mut state = state.borrow_mut();
        state.underrun_count = state.underrun_count.saturating_add(1);
        state.underrun_count
    })
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
                if let Some(volumes) = state.trigger_volumes(pad) {
                    sequencer.set_trigger_volumes(pad, volumes);
                }
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
    let mut mute_button = MuteButtonState::new();
    let mut next_scan = Instant::now();
    let mut encoder_acceleration = UiEncoderAcceleration::new();

    loop {
        if let Ok(direction) = with_deadline(next_scan, encoder.read()).await {
            let ui_state = ui.lock(|state| *state.borrow());
            let controller = ui_state.controller;
            // Return acts on its physical edge immediately: detents received
            // during its debounce window are discarded rather than editing or
            // queuing a preview just before navigation resets.
            if !keys[RETURN_KEY_INDEX].is_low()
                && !controller.key_suppressed(RETURN_KEY_INDEX)
                && !controller.encoder_suppressed()
            {
                let encoder_button_held = encoder_button.is_low();
                let target = controller.encoder_target(ui_state.volume_pressed);
                let direction_delta = match direction {
                    Direction::Clockwise => 1,
                    Direction::CounterClockwise => -1,
                };
                let unaccelerated = matches!(
                    target,
                    UiEncoderTarget::Root
                        | UiEncoderTarget::BeatsNone
                        | UiEncoderTarget::PatternRepeat(_)
                        | UiEncoderTarget::PatternAll(_)
                        | UiEncoderTarget::PatternNone
                        | UiEncoderTarget::Sample(_)
                        | UiEncoderTarget::SongStatus
                        | UiEncoderTarget::ResetAll
                ) || (target == UiEncoderTarget::Songs
                    && !matches!(controller.songs_view(), SongsView::Browser { .. }));
                let delta = if unaccelerated {
                    encoder_acceleration = UiEncoderAcceleration::new();
                    direction_delta
                } else {
                    encoder_acceleration.update(Instant::now().as_millis(), target, direction_delta)
                };
                match target {
                    UiEncoderTarget::Root => ui.lock(|state| {
                        state.borrow_mut().controller.rotate_root(delta);
                    }),
                    UiEncoderTarget::Pattern(pad) | UiEncoderTarget::PatternAll(pad) => {
                        let division = shared.lock(|state| {
                            state
                                .borrow()
                                .effective_pattern_steps(pad)
                                .unwrap_or(0)
                                .saturating_add(1)
                        });
                        ui.lock(|state| {
                            state
                                .borrow_mut()
                                .controller
                                .rotate_pattern(pad, division, delta);
                        });
                    }
                    UiEncoderTarget::PatternRepeat(pad) => shared.lock(|state| {
                        let mut state = state.borrow_mut();
                        let current = state.pattern_repeat(pad).unwrap_or(1);
                        let adjusted = i32::from(current).saturating_add(direction_delta).clamp(
                            1,
                            i32::from(looptic::max_pattern_repeats(state.desired_beats[pad])),
                        ) as u16;
                        let _ = state.set_pattern_repeat(pad, adjusted);
                    }),
                    UiEncoderTarget::PatternNone => {}
                    UiEncoderTarget::Sample(Some(pad)) => shared.lock(|state| {
                        let mut state = state.borrow_mut();
                        if let Some(current) = state.pad_sample(pad) {
                            let selected = adjust_sample_selection(current, delta);
                            if state.set_pad_sample(pad, selected)
                                && let Some(preview) = sample_selection_preview_request(
                                    pad,
                                    current,
                                    selected,
                                    encoder_button_held,
                                )
                            {
                                let _ = state.queue_preview(preview);
                            }
                        }
                    }),
                    UiEncoderTarget::Sample(None) => {}
                    UiEncoderTarget::ResetAll => ui.lock(|state| {
                        state.borrow_mut().controller.rotate_reset_choice(delta);
                    }),
                    UiEncoderTarget::Songs => ui.lock(|state| {
                        state.borrow_mut().controller.rotate_songs(delta);
                    }),
                    UiEncoderTarget::SongStatus => {}
                    UiEncoderTarget::Volume(target) => shared.lock(|state| {
                        let _ = state.borrow_mut().adjust_volume(target, delta);
                    }),
                    UiEncoderTarget::PatternVolume(target) => shared.lock(|state| {
                        let _ = state.borrow_mut().adjust_pattern_volume(target, delta);
                    }),
                    UiEncoderTarget::CycleGlobal => shared.lock(|state| {
                        let mut state = state.borrow_mut();
                        let adjusted = adjust_base_interval(state.base_interval_ms, delta);
                        let _ = state.set_base_interval_ms(adjusted);
                    }),
                    UiEncoderTarget::BeatsNone => {}
                    UiEncoderTarget::BeatsPad(pad) => {
                        let division = shared.lock(|state| {
                            let mut state = state.borrow_mut();
                            let adjusted = adjust_beat_multiplier(state.desired_beats[pad], delta);
                            let _ = state.set_desired_beats(pad, adjusted);
                            state
                                .effective_pattern_steps(pad)
                                .unwrap_or(0)
                                .saturating_add(1)
                        });
                        ui.lock(|state| {
                            state
                                .borrow_mut()
                                .controller
                                .clamp_pattern_cursor(pad, division);
                        });
                    }
                    UiEncoderTarget::CyclePadLength(pad) => shared.lock(|state| {
                        let mut state = state.borrow_mut();
                        let current = state.pad_cycle_length_override_ms(pad).unwrap_or(0);
                        let adjusted = adjust_pad_cycle_length(current, delta);
                        let _ = state.set_pad_cycle_length_ms(pad, adjusted);
                    }),
                    UiEncoderTarget::Light => shared.lock(|state| {
                        let mut state = state.borrow_mut();
                        state.led_brightness_percent =
                            adjust_led_brightness(state.led_brightness_percent, delta);
                    }),
                }
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

            let physical_encoder_pressed = encoder_button.is_low();
            let changes = debouncer.update(raw_mask);
            let button_changes =
                encoder_button_debouncer.update(u16::from(physical_encoder_pressed));
            let stable_mask = debouncer.stable_mask();
            let debounced_encoder_pressed = encoder_button_debouncer.stable_mask() & 1 != 0;
            let mut next_ui = ui.lock(|state| *state.borrow());
            next_ui
                .controller
                .update_suppression(raw_mask, physical_encoder_pressed);

            let mute_bit = 1_u16 << MUTE_KEY_INDEX;
            let return_bit = 1_u16 << RETURN_KEY_INDEX;
            let now_ms = now.as_millis();

            let return_pressed = changes.pressed & return_bit != 0
                && !next_ui.controller.key_suppressed(RETURN_KEY_INDEX);
            let mute_action = resolve_mute_scan(
                &mut mute_button,
                return_pressed,
                changes.released & mute_bit != 0,
                now_ms,
            );
            if return_pressed {
                if matches!(mute_action, Some(MuteScanAction::Cancel(_))) {
                    shared.lock(|state| {
                        state.borrow_mut().cancel_mute_gesture();
                    });
                }
                next_ui
                    .controller
                    .return_to_root(raw_mask, physical_encoder_pressed);
                next_ui.volume_pressed = false;
                encoder_acceleration = UiEncoderAcceleration::new();
            } else {
                let storage_busy = matches!(
                    next_ui.controller.song_status(),
                    Some(SongUiStatus::Busy { .. } | SongUiStatus::Formatting { .. })
                );
                if storage_busy {
                    if matches!(mute_action, Some(MuteScanAction::Release(_))) {
                        shared.lock(|state| {
                            state.borrow_mut().cancel_mute_gesture();
                        });
                    }
                    next_ui.volume_pressed = false;
                    let revision = shared.lock(|state| state.borrow().song_revision);
                    next_ui.song_library.dirty = revision != next_ui.clean_song_revision;
                    ui.lock(|state| *state.borrow_mut() = next_ui);
                    next_scan += KEY_SCAN_INTERVAL;
                    if next_scan <= now {
                        next_scan = now + KEY_SCAN_INTERVAL;
                    }
                    continue;
                }
                if let Some(MuteScanAction::Release(release)) = mute_action {
                    shared.lock(|state| {
                        state.borrow_mut().end_mute_gesture(release);
                    });
                }
                let allowed_pressed = changes.pressed & !next_ui.controller.suppressed_keys();
                let pressed_beats = allowed_pressed & looptic::BEAT_PAD_MASK;
                if pressed_beats != 0 {
                    // Deterministic simultaneous-press policy: the lowest
                    // logical beat key wins this scan.
                    let pad = pressed_beats.trailing_zeros() as usize;
                    next_ui.controller.press_voice(pad);
                    if let Some(selected) = next_ui.controller.selected_pad() {
                        let division = shared.lock(|state| {
                            state
                                .borrow()
                                .effective_pattern_steps(selected)
                                .unwrap_or(0)
                                .saturating_add(1)
                        });
                        next_ui.controller.clamp_pattern_cursor(selected, division);
                    }
                }

                if allowed_pressed & mute_bit != 0 {
                    let target = MuteTarget::for_selected_pad(next_ui.controller.selected_pad());
                    if mute_button.press(target, now_ms) {
                        shared.lock(|state| {
                            state.borrow_mut().begin_mute_gesture(target);
                        });
                    }
                }

                let volume_pressed = stable_mask & (1_u16 << VOLUME_KEY_INDEX) != 0
                    && !next_ui.controller.key_suppressed(VOLUME_KEY_INDEX);
                let encoder_pressed =
                    debounced_encoder_pressed && !next_ui.controller.encoder_suppressed();
                next_ui.volume_pressed = volume_pressed;

                if button_changes.pressed & 1 != 0 && encoder_pressed && !volume_pressed {
                    let selected_division = next_ui.controller.selected_pad().map(|pad| {
                        shared.lock(|state| {
                            state
                                .borrow()
                                .effective_pattern_steps(pad)
                                .unwrap_or(0)
                                .saturating_add(1)
                        })
                    });
                    let songs_page = next_ui.controller.page() == UiPage::Songs;
                    let action = next_ui.controller.press_encoder(selected_division);
                    if songs_page {
                        encoder_acceleration = UiEncoderAcceleration::new();
                    }
                    match action {
                        Some(UiAction::Pattern(action)) => {
                            if apply_pattern_editor_action(shared, action) {
                                encoder_acceleration = UiEncoderAcceleration::new();
                            }
                        }
                        Some(UiAction::ResetConfirmed) => {
                            mute_button.cancel();
                            let revision = shared.lock(|state| {
                                let mut state = state.borrow_mut();
                                state.reset_musical_state();
                                state.song_revision
                            });
                            next_ui.song_library.current_slot = None;
                            next_ui.song_library.dirty = false;
                            next_ui.clean_song_revision = revision;
                            encoder_acceleration = UiEncoderAcceleration::new();
                        }
                        Some(UiAction::Song(operation)) => {
                            if operation == SongStorageOperation::SaveCurrent
                                && next_ui.song_library.current_slot.is_none()
                            {
                                next_ui.controller.open_save_as(None);
                            } else {
                                mute_button.cancel();
                                shared.lock(|state| {
                                    state.borrow_mut().cancel_mute_gesture();
                                });
                                next_ui.volume_pressed = false;
                                // The storage task consumes this latest command.
                                // Busy routing prevents a second command.
                                next_ui.pending_song_operation = Some(operation);
                            }
                            encoder_acceleration = UiEncoderAcceleration::new();
                        }
                        None => {}
                    }
                }
            }
            let revision = shared.lock(|state| state.borrow().song_revision);
            next_ui.song_library.dirty = revision != next_ui.clean_song_revision;
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
        let selected_pad = ui_state.controller.selected_pad();
        let mute_target = MuteTarget::for_selected_pad(selected_pad);
        let volume_target = ui_state.controller.encoder_target(true);
        let (playback_frame, triggers, underruns, brightness, mute_active, volume) =
            shared.lock(|state| {
                let state = state.borrow();
                (
                    state.playback_frame,
                    state.latest_trigger_frames,
                    state.underrun_count,
                    state.led_brightness_percent,
                    state.mute_indicator_active(mute_target).unwrap_or(false),
                    match volume_target {
                        UiEncoderTarget::PatternVolume(target) => {
                            state.pattern_volume_percent(target).unwrap_or(0)
                        }
                        UiEncoderTarget::Volume(target) => {
                            state.volume_percent(target).unwrap_or(0)
                        }
                        _ => 0,
                    },
                )
            });
        let light_preview = ui_state.controller.page() == UiPage::Light;

        let mut colors = [RGB8::default(); KEY_COUNT];
        for pad in 0..BEAT_PAD_COUNT {
            let trigger_active = led_pulse_active(playback_frame, triggers[pad], SAMPLE_RATE / 10);
            let (r, g, b) = voice_led_color(
                pad,
                brightness,
                selected_pad.is_some(),
                selected_pad == Some(pad),
                trigger_active,
                light_preview,
            );
            colors[pad] = RGB8 { r, g, b };
        }
        let (r, g, b) = mute_led_color(mute_active, brightness);
        colors[MUTE_KEY_INDEX] = RGB8 { r, g, b };
        let (r, g, b) = volume_led_color(volume, brightness);
        colors[VOLUME_KEY_INDEX] = RGB8 { r, g, b };
        let (r, g, b) = return_led_color(brightness);
        colors[RETURN_KEY_INDEX] = RGB8 { r, g, b };
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
    let (
        beats,
        repeats,
        pattern_steps,
        cursor,
        window,
        fill_state,
        all_volume,
        enabled_rows,
        volume_rows,
    ) = shared.lock(|state| {
        let state = state.borrow();
        let beats = state.desired_beats[pad];
        let repeats = state.pattern_repeat(pad).unwrap_or(1);
        let pattern_steps = state.effective_pattern_steps(pad).unwrap_or(0);
        let cursor = cursor.min(pattern_steps.saturating_add(1));
        let window = scroll_menu_window(
            usize::from(cursor),
            usize::from(pattern_steps) + 2,
            usize::from(VISIBLE_ROWS),
        );
        let mut enabled_rows = [false; VISIBLE_ROWS as usize];
        let mut volume_rows = [0_u8; VISIBLE_ROWS as usize];
        let mut fill_state = PatternFillState::Empty;
        let mut all_volume = 0;
        if let Some(pattern) = state.pattern(pad) {
            fill_state = pattern.fill_state();
            for row in 0..window.item_rows {
                let entry = window.start + row;
                if entry >= 2 && entry <= usize::from(pattern_steps) + 1 {
                    let step = entry - 2;
                    enabled_rows[row] = pattern
                        .step_enabled(step as u16, pattern_steps)
                        .unwrap_or(false);
                    volume_rows[row] = state.trigger_volume(pad, step).unwrap_or(0);
                }
            }
        }
        if let Some(volumes) = state.trigger_volumes(pad) {
            all_volume = volumes.average_percent();
        }
        (
            beats,
            repeats,
            pattern_steps,
            cursor,
            window,
            fill_state,
            all_volume,
            enabled_rows,
            volume_rows,
        )
    });

    let mut header: String<24> = String::new();
    let _ = write!(&mut header, "LoopTic P{} {}x{}", pad + 1, beats, repeats);
    let _ = Text::with_baseline(&header, Point::new(0, 0), style, Baseline::Top).draw(display);

    if window.more_above {
        draw_scroll_triangle(display, 14, true);
    }
    for row in 0..window.item_rows {
        let entry = window.start + row;
        let marker = if entry == usize::from(cursor) {
            '>'
        } else {
            ' '
        };
        let mut line: String<20> = String::new();
        if entry == 0 {
            let _ = write!(&mut line, "{} Cycles {}x", marker, repeats);
        } else if entry == 1 {
            let state = match fill_state {
                PatternFillState::Empty => "off",
                PatternFillState::Full => "ON",
                PatternFillState::Mixed => "mix",
            };
            let _ = write!(&mut line, "{} All {} avg{}%", marker, state, all_volume);
        } else {
            let state = if enabled_rows[row] { "ON" } else { "off" };
            let _ = write!(
                &mut line,
                "{} {:04} {} {}%",
                marker,
                entry - 1,
                state,
                volume_rows[row]
            );
        }
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 14 + row as i32 * 10),
            style,
            Baseline::Top,
        )
        .draw(display);
    }
    if window.more_below {
        draw_scroll_triangle(display, 14 + (window.item_rows - 1) as i32 * 10, false);
    }

    if pattern_steps == 0 {
        let _ = Text::with_baseline("No triggers", Point::new(0, 38), style, Baseline::Top)
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

fn draw_root_menu<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    highlighted: RootMode,
    selected_pad: Option<usize>,
    current_song: Option<SongSlot>,
    song_dirty: bool,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let mut header: String<24> = String::new();
    if let Some(slot) = current_song {
        let _ = write!(
            &mut header,
            "LoopTic {:03}{}",
            slot.number(),
            if song_dirty { "*" } else { "" }
        );
    } else {
        let _ = write!(
            &mut header,
            "LoopTic Unsaved{}",
            if song_dirty { "*" } else { "" }
        );
    }
    if let Some(pad) = selected_pad {
        let _ = write!(&mut header, " P{}", pad + 1);
    }
    let _ = Text::with_baseline(&header, Point::new(0, 0), style, Baseline::Top).draw(display);

    const VISIBLE_ROWS: usize = 5;
    let window = scroll_menu_window(highlighted.index(), RootMode::COUNT, VISIBLE_ROWS);
    if window.more_above {
        draw_scroll_triangle(display, 14, true);
    }
    for (row, mode) in RootMode::ALL[window.start..window.start + window.item_rows]
        .iter()
        .copied()
        .enumerate()
    {
        let marker = if mode == highlighted { '>' } else { ' ' };
        let label = match mode {
            RootMode::Pattern => "Pattern",
            RootMode::Beats => "Beats",
            RootMode::CycleLength => "Cycle length",
            RootMode::Sample => "Sample",
            RootMode::Light => "Light",
            RootMode::Save => "Save",
            RootMode::Songs => "Songs",
            RootMode::ResetAll => "Reset all",
        };
        let mut line: String<24> = String::new();
        let _ = write!(&mut line, "{} {}", marker, label);
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 14 + row as i32 * 10),
            style,
            Baseline::Top,
        )
        .draw(display);
    }
    if window.more_below {
        draw_scroll_triangle(display, 14 + (window.item_rows - 1) as i32 * 10, false);
    }
}

fn draw_songs_menu<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    selected: SongMenuOperation,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ =
        Text::with_baseline("LoopTic Songs", Point::new(0, 0), style, Baseline::Top).draw(display);
    for (row, operation) in SongMenuOperation::ALL.iter().copied().enumerate() {
        let marker = if operation == selected { '>' } else { ' ' };
        let label = match operation {
            SongMenuOperation::Load => "Load",
            SongMenuOperation::SaveAs => "Save as",
            SongMenuOperation::Copy => "Copy",
            SongMenuOperation::Delete => "Delete",
        };
        let mut line: String<20> = String::new();
        let _ = write!(&mut line, "{} {}", marker, label);
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 16 + row as i32 * 12),
            style,
            Baseline::Top,
        )
        .draw(display);
    }
}

fn draw_song_browser<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    purpose: SongBrowserPurpose,
    selected: SongSlot,
    occupied: SongSlotOccupancy,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let title = match purpose {
        SongBrowserPurpose::Load => "Load song",
        SongBrowserPurpose::SaveAs => "Save as",
        SongBrowserPurpose::CopySource => "Copy from",
        SongBrowserPurpose::CopyDestination { .. } => "Copy to",
        SongBrowserPurpose::Delete => "Delete song",
    };
    let _ = Text::with_baseline(title, Point::new(0, 0), style, Baseline::Top).draw(display);

    const VISIBLE_ROWS: usize = 4;
    let window = scroll_menu_window(selected.index(), looptic::SONG_SLOT_COUNT, VISIBLE_ROWS);
    if window.more_above {
        draw_scroll_triangle(display, 16, true);
    }
    for row in 0..window.item_rows {
        let slot = SongSlot::from_index(window.start + row).unwrap_or_default();
        let marker = if slot == selected { '>' } else { ' ' };
        let stored = if occupied.contains(slot) { '*' } else { '-' };
        let mut line: String<24> = String::new();
        let _ = write!(
            &mut line,
            "{}{:03}{} {}",
            marker,
            slot.number(),
            stored,
            slot.animal_name()
        );
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 16 + row as i32 * 12),
            style,
            Baseline::Top,
        )
        .draw(display);
    }
    if window.more_below {
        draw_scroll_triangle(display, 16 + (window.item_rows - 1) as i32 * 12, false);
    }
}

fn draw_scroll_triangle<D>(display: &mut D, row_y: i32, points_up: bool)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let (tip_y, base_y) = if points_up {
        (row_y + 1, row_y + 7)
    } else {
        (row_y + 7, row_y + 1)
    };
    let _ = Triangle::new(
        Point::new(3, tip_y),
        Point::new(0, base_y),
        Point::new(6, base_y),
    )
    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
    .draw(display);
}

fn song_operation_label(operation: SongStorageOperation) -> &'static str {
    match operation {
        SongStorageOperation::Format => "Format",
        SongStorageOperation::SaveCurrent | SongStorageOperation::SaveAs { .. } => "Save",
        SongStorageOperation::Load { .. } => "Load",
        SongStorageOperation::Copy { .. } => "Copy",
        SongStorageOperation::Delete { .. } => "Delete",
    }
}

fn draw_song_confirmation<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    operation: SongStorageOperation,
    selected: SongConfirmChoice,
    destination_occupied: bool,
    live_song_dirty: bool,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let mut header: String<24> = String::new();
    let _ = write!(&mut header, "{} song?", song_operation_label(operation));
    let _ = Text::with_baseline(&header, Point::new(0, 0), style, Baseline::Top).draw(display);
    let mut line: String<24> = String::new();
    match operation {
        SongStorageOperation::Format => {
            let _ = line.push_str("Erase all songs");
        }
        SongStorageOperation::Copy {
            source,
            destination,
        } => {
            let _ = write!(
                &mut line,
                "{:03} -> {:03}",
                source.number(),
                destination.number()
            );
        }
        _ => {
            if let Some(slot) = operation.destination_slot() {
                let _ = write!(&mut line, "{:03} {}", slot.number(), slot.animal_name());
            }
        }
    }
    let _ = Text::with_baseline(&line, Point::new(0, 13), style, Baseline::Top).draw(display);
    let warning = if destination_occupied {
        Some("Overwrite stored")
    } else if matches!(operation, SongStorageOperation::Load { .. }) && live_song_dirty {
        Some("Unsaved changes!")
    } else {
        None
    };
    if let Some(warning) = warning {
        let _ = Text::with_baseline(warning, Point::new(0, 25), style, Baseline::Top).draw(display);
    }
    for (row, (choice, label)) in [
        (SongConfirmChoice::Cancel, "Cancel"),
        (SongConfirmChoice::Confirm, "Confirm"),
    ]
    .iter()
    .enumerate()
    {
        let marker = if *choice == selected { '>' } else { ' ' };
        let mut line: String<16> = String::new();
        let _ = write!(&mut line, "{} {}", marker, label);
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 37 + row as i32 * 13),
            style,
            Baseline::Top,
        )
        .draw(display);
    }
}

fn draw_song_status<D>(display: &mut D, style: MonoTextStyle<'_, BinaryColor>, status: SongUiStatus)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ =
        Text::with_baseline("LoopTic Songs", Point::new(0, 0), style, Baseline::Top).draw(display);
    let mut first: String<24> = String::new();
    let mut second: String<24> = String::new();
    match status {
        SongUiStatus::Busy { operation } => {
            let _ = write!(
                &mut first,
                "{} in progress",
                song_operation_label(operation)
            );
            let _ = second.push_str("Please wait");
        }
        SongUiStatus::Formatting { percent } => {
            let _ = first.push_str("Formatting storage");
            let _ = write!(&mut second, "{}% complete", percent);
        }
        SongUiStatus::Success { operation } => {
            let _ = write!(&mut first, "{} complete", song_operation_label(operation));
            if let Some(slot) = operation.destination_slot() {
                let _ = write!(&mut second, "{:03} {}", slot.number(), slot.animal_name());
            }
        }
        SongUiStatus::NoChanges { slot } => {
            let _ = first.push_str("No changes");
            let _ = write!(&mut second, "{:03} {}", slot.number(), slot.animal_name());
        }
        SongUiStatus::Empty { slot } => {
            let _ = first.push_str("Slot is empty");
            let _ = write!(&mut second, "{:03} {}", slot.number(), slot.animal_name());
        }
        SongUiStatus::UnsupportedVersion {
            slot,
            found,
            supported,
        } => {
            let _ = first.push_str("Unsupported format");
            if let Some(slot) = slot {
                let _ = write!(
                    &mut second,
                    "{:03} v{} (need v{})",
                    slot.number(),
                    found,
                    supported
                );
            } else {
                let _ = write!(&mut second, "v{} (need v{})", found, supported);
            }
        }
        SongUiStatus::UnsupportedStorage { found, supported } => {
            let _ = first.push_str("Storage unsupported");
            if found == supported {
                let _ = second.push_str("layout mismatch");
            } else {
                let _ = write!(&mut second, "format {} (need {})", found, supported);
            }
        }
        SongUiStatus::Corrupt { slot } => {
            let _ = first.push_str(if slot.is_some() {
                "Song data corrupt"
            } else {
                "Storage corrupt"
            });
            if let Some(slot) = slot {
                let _ = write!(&mut second, "Slot {:03}", slot.number());
            }
        }
        SongUiStatus::Failed { operation } => {
            let _ = write!(&mut first, "{} failed", song_operation_label(operation));
            let _ = second.push_str("Result uncertain");
        }
        SongUiStatus::Unavailable => {
            let _ = first.push_str("Storage unavailable");
            let _ = second.push_str("Reboot and retry");
        }
    }
    let _ = Text::with_baseline(&first, Point::new(0, 18), style, Baseline::Top).draw(display);
    let _ = Text::with_baseline(&second, Point::new(0, 34), style, Baseline::Top).draw(display);
    if !matches!(
        status,
        SongUiStatus::Busy { .. } | SongUiStatus::Formatting { .. }
    ) {
        let hint = if matches!(status, SongUiStatus::Failed { .. }) {
            "Reboot and verify"
        } else {
            "Push to close"
        };
        let _ = Text::with_baseline(hint, Point::new(0, 50), style, Baseline::Top).draw(display);
    }
}

fn draw_select_voice<D>(display: &mut D, style: MonoTextStyle<'_, BinaryColor>, mode: &str)
where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ = Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top).draw(display);
    let _ = Text::with_baseline(mode, Point::new(0, 16), style, Baseline::Top).draw(display);
    let _ =
        Text::with_baseline("Select voice", Point::new(0, 30), style, Baseline::Top).draw(display);
}

fn draw_reset_menu<D>(
    display: &mut D,
    style: MonoTextStyle<'_, BinaryColor>,
    selected: ResetAllChoice,
) where
    D: DrawTarget<Color = BinaryColor>,
{
    let _ = Text::with_baseline("LoopTic Reset all", Point::new(0, 0), style, Baseline::Top)
        .draw(display);
    for (row, (choice, label)) in [
        (ResetAllChoice::Cancel, "Cancel"),
        (ResetAllChoice::Reset, "Reset"),
    ]
    .iter()
    .enumerate()
    {
        let marker = if *choice == selected { '>' } else { ' ' };
        let mut line: String<16> = String::new();
        let _ = write!(&mut line, "{} {}", marker, label);
        let _ = Text::with_baseline(
            &line,
            Point::new(0, 18 + row as i32 * 16),
            style,
            Baseline::Top,
        )
        .draw(display);
    }
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
        let model = ui_state
            .controller
            .display_model_with_library(ui_state.volume_pressed, ui_state.song_library);
        let (displayed_value, pattern_revision) = shared.lock(|state| {
            let state = state.borrow();
            match model {
                UiDisplayModel::Volume { target } => {
                    (u32::from(state.volume_percent(target).unwrap_or(0)), 0)
                }
                UiDisplayModel::PatternVolume { target } => (
                    u32::from(state.pattern_volume_percent(target).unwrap_or(0)),
                    state.pattern_revision,
                ),
                UiDisplayModel::BeatsPad { pad } => (u32::from(state.desired_beats[pad]), 0),
                UiDisplayModel::CycleGlobal => (state.base_interval_ms, 0),
                UiDisplayModel::CyclePadLength { pad } => {
                    (state.pad_cycle_length_override_ms(pad).unwrap_or(0), 0)
                }
                UiDisplayModel::PatternEditor { pad, .. }
                | UiDisplayModel::PatternAll { pad, .. } => {
                    (u32::from(state.desired_beats[pad]), state.pattern_revision)
                }
                UiDisplayModel::PatternRepeat { pad } => (
                    u32::from(state.pattern_repeat(pad).unwrap_or(1)),
                    state.pattern_revision,
                ),
                UiDisplayModel::SamplePad { pad } => (
                    state
                        .pad_sample(pad)
                        .map_or(0, |sample| sample.index() as u32),
                    0,
                ),
                UiDisplayModel::Light => (u32::from(state.led_brightness_percent), 0),
                UiDisplayModel::Root { .. }
                | UiDisplayModel::PatternSelectVoice
                | UiDisplayModel::BeatsSelectVoice
                | UiDisplayModel::SampleSelectVoice
                | UiDisplayModel::SongsMenu { .. }
                | UiDisplayModel::SongBrowser { .. }
                | UiDisplayModel::SongConfirmation { .. }
                | UiDisplayModel::SongStatus { .. }
                | UiDisplayModel::ResetAll { .. } => (0, 0),
            }
        });
        let value = DisplayStateKey {
            model,
            displayed_value,
            pattern_revision,
        };

        if last_value != Some(value) {
            display.clear();
            match model {
                UiDisplayModel::Root {
                    highlighted,
                    selected_pad,
                    current_song,
                    song_dirty,
                } => draw_root_menu(
                    &mut display,
                    style,
                    highlighted,
                    selected_pad,
                    current_song,
                    song_dirty,
                ),
                UiDisplayModel::Volume { target } => {
                    let _ = Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                        .draw(&mut display);
                    let mut line: String<24> = String::new();
                    match target {
                        VolumeTarget::Global => {
                            let _ = write!(&mut line, "Master {}%", displayed_value);
                        }
                        VolumeTarget::Pad(pad) => {
                            let _ = write!(&mut line, "P{} Vol {}%", pad + 1, displayed_value);
                        }
                    }
                    let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                        .draw(&mut display);
                }
                UiDisplayModel::PatternVolume { target } => {
                    let _ = Text::with_baseline(
                        "LoopTic Pattern",
                        Point::new(0, 0),
                        style,
                        Baseline::Top,
                    )
                    .draw(&mut display);
                    let mut line: String<24> = String::new();
                    match target {
                        PatternVolumeTarget::All { pad } => {
                            let _ = write!(&mut line, "P{} All avg {}%", pad + 1, displayed_value);
                        }
                        PatternVolumeTarget::Step { pad, step } => {
                            let _ = write!(
                                &mut line,
                                "P{} T{:03} {}%",
                                pad + 1,
                                step + 1,
                                displayed_value
                            );
                        }
                    }
                    let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                        .draw(&mut display);
                }
                UiDisplayModel::PatternSelectVoice => {
                    draw_select_voice(&mut display, style, "Pattern");
                }
                UiDisplayModel::PatternRepeat { pad } => {
                    let beats = shared.lock(|state| state.borrow().desired_beats[pad]);
                    let maximum = looptic::max_pattern_repeats(beats);
                    let _ = Text::with_baseline(
                        "LoopTic Cycles",
                        Point::new(0, 0),
                        style,
                        Baseline::Top,
                    )
                    .draw(&mut display);
                    let mut line: String<24> = String::new();
                    let _ = write!(
                        &mut line,
                        "P{} {}x (1-{})",
                        pad + 1,
                        displayed_value,
                        maximum
                    );
                    let _ = Text::with_baseline(&line, Point::new(0, 18), style, Baseline::Top)
                        .draw(&mut display);
                    let _ = Text::with_baseline(
                        "Push/Return done",
                        Point::new(0, 38),
                        style,
                        Baseline::Top,
                    )
                    .draw(&mut display);
                }
                UiDisplayModel::PatternEditor { pad, cursor } => {
                    draw_pattern_editor(&mut display, style, shared, pad, cursor);
                }
                UiDisplayModel::PatternAll { pad, choice } => {
                    draw_pattern_all_menu(&mut display, style, pad, choice);
                }
                UiDisplayModel::BeatsSelectVoice => {
                    draw_select_voice(&mut display, style, "Beats");
                }
                UiDisplayModel::BeatsPad { pad } => {
                    let mut header: String<24> = String::new();
                    let _ = write!(&mut header, "LoopTic P{} Beats", pad + 1);
                    let _ = Text::with_baseline(&header, Point::new(0, 0), style, Baseline::Top)
                        .draw(&mut display);
                    let mut line: String<24> = String::new();
                    let _ = write!(&mut line, "Beats {}", displayed_value);
                    let _ = Text::with_baseline(&line, Point::new(0, 18), style, Baseline::Top)
                        .draw(&mut display);
                }
                UiDisplayModel::CycleGlobal => {
                    let _ = Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                        .draw(&mut display);
                    let mut line: String<24> = String::new();
                    let _ = Text::with_baseline(
                        "Cycle length",
                        Point::new(0, 16),
                        style,
                        Baseline::Top,
                    )
                    .draw(&mut display);
                    let _ = write!(&mut line, "{} ms", displayed_value);
                    let _ = Text::with_baseline(&line, Point::new(0, 30), style, Baseline::Top)
                        .draw(&mut display);
                }
                UiDisplayModel::CyclePadLength { pad } => {
                    let mut header: String<24> = String::new();
                    let _ = write!(&mut header, "LoopTic P{} Cycle", pad + 1);
                    let _ = Text::with_baseline(&header, Point::new(0, 0), style, Baseline::Top)
                        .draw(&mut display);
                    let mut line: String<24> = String::new();
                    if displayed_value == 0 {
                        let _ = write!(&mut line, "Length 0 (Global)");
                    } else {
                        let _ = write!(&mut line, "Length {} ms", displayed_value);
                    }
                    let _ = Text::with_baseline(&line, Point::new(0, 18), style, Baseline::Top)
                        .draw(&mut display);
                    let _ =
                        Text::with_baseline("0 = Global", Point::new(0, 38), style, Baseline::Top)
                            .draw(&mut display);
                }
                UiDisplayModel::SampleSelectVoice => {
                    draw_select_voice(&mut display, style, "Sample");
                }
                UiDisplayModel::SamplePad { pad } => {
                    let _ = Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
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
                UiDisplayModel::Light => {
                    let _ = Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                        .draw(&mut display);
                    let mut line: String<24> = String::new();
                    let _ = write!(&mut line, "Light {}%", displayed_value);
                    let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                        .draw(&mut display);
                }
                UiDisplayModel::SongsMenu { selected } => {
                    draw_songs_menu(&mut display, style, selected);
                }
                UiDisplayModel::SongBrowser {
                    purpose,
                    slot,
                    occupied,
                } => {
                    draw_song_browser(&mut display, style, purpose, slot, occupied);
                }
                UiDisplayModel::SongConfirmation {
                    operation,
                    choice,
                    destination_occupied,
                    live_song_dirty,
                } => draw_song_confirmation(
                    &mut display,
                    style,
                    operation,
                    choice,
                    destination_occupied,
                    live_song_dirty,
                ),
                UiDisplayModel::SongStatus { status } => {
                    draw_song_status(&mut display, style, status);
                }
                UiDisplayModel::ResetAll { choice } => {
                    draw_reset_menu(&mut display, style, choice);
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
        spawner.spawn(storage_task(p.FLASH, shared, ui).unwrap());
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
