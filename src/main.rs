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
use embassy_time::{Delay, Duration, Instant, Ticker, Timer, with_deadline};
use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_6X10};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Baseline, Text};
use fixed::types::U24F8;
use heapless::String;
use looptic::{
    AUDIO_BLOCK_FRAMES, EncoderAcceleration, EncoderTarget, HeldPadSelection, KeyDebouncer,
    PAD_COUNT, SAMPLE_RATE, SILENCE_PWM_WORD, Sequencer, SharedState, WavPcm16,
    apply_encoder_delta, colorwheel, led_pulse_active, scale_color,
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
}

struct OledResources {
    spi: Peri<'static, SPI1>,
    sck: Peri<'static, PIN_26>,
    mosi: Peri<'static, PIN_27>,
    cs: Peri<'static, PIN_22>,
    reset: Peri<'static, PIN_23>,
    dc: Peri<'static, PIN_24>,
}

static SHARED: StaticCell<Shared> = StaticCell::new();
static UI_SHARED: StaticCell<UiShared> = StaticCell::new();
static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
static EXECUTOR_LOW: StaticCell<Executor> = StaticCell::new();
static AUDIO_DMA_FRONT: StaticCell<[u32; AUDIO_BLOCK_FRAMES]> = StaticCell::new();
static AUDIO_DMA_BACK: StaticCell<[u32; AUDIO_BLOCK_FRAMES]> = StaticCell::new();
static FATAL_FAULT: AtomicBool = AtomicBool::new(false);

const KICK_WAV: &[u8] = include_bytes!("../samples/00_kick02.wav");
const OPEN_HAT_WAV: &[u8] = include_bytes!("../samples/02_ho02.wav");
const AUDIO_CLOCK_DIVIDER_BITS: u32 = 667;
const KEY_SCAN_INTERVAL: Duration = Duration::from_millis(1);
const DISPLAY_INTERVAL: Duration = Duration::from_millis(34);
const LED_INTERVAL: Duration = Duration::from_millis(5);

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
    let (initial_beats, initial_base_interval) = shared.lock(|state| {
        let state = state.borrow();
        (state.desired_beats, state.base_interval_ms)
    });
    sequencer.apply_timing(&initial_beats, initial_base_interval, front_start);
    let report = sequencer.render(front_start, front);
    publish_render(shared, &report);

    // Joined TX mode provides eight words of elasticity between DMA completions.
    for _ in 0..8 {
        sm.tx().push(SILENCE_PWM_WORD);
    }
    // Ignore the intentional empty-FIFO condition that existed before startup.
    let _ = sm.tx().stalled();
    sm.set_enable(true);

    loop {
        let transfer = sm.tx().dma_push(&mut dma, front, false);

        let back_start = front_start.wrapping_add(AUDIO_BLOCK_FRAMES as u64);
        let (beats, base_interval) = shared.lock(|state| {
            let state = state.borrow();
            (state.desired_beats, state.base_interval_ms)
        });
        sequencer.apply_timing(&beats, base_interval, back_start);
        let report = sequencer.render(back_start, back);
        publish_render(shared, &report);

        transfer.await;

        if sm.tx().stalled() {
            shared.lock(|state| {
                let mut state = state.borrow_mut();
                state.underrun_count = state.underrun_count.saturating_add(1);
            });
        }

        front_start = back_start;
        shared.lock(|state| state.borrow_mut().playback_frame = front_start);
        mem::swap(&mut front, &mut back);
    }
}

fn publish_render(shared: &Shared, report: &looptic::RenderReport) {
    shared.lock(|state| {
        let mut state = state.borrow_mut();
        for (pad, trigger) in report.latest_visual_triggers.iter().enumerate() {
            if let Some(frame) = trigger {
                state.latest_trigger_frames[pad] = *frame;
            }
        }
    });
}

#[embassy_executor::task]
async fn controls_task(
    mut keys: [Input<'static>; PAD_COUNT],
    mut encoder: PioEncoder<'static, PIO1, 1>,
    encoder_button: Input<'static>,
    shared: &'static Shared,
    ui: &'static UiShared,
) {
    let mut debouncer = KeyDebouncer::new(5);
    let mut encoder_button_debouncer = KeyDebouncer::new(5);
    let mut selection = HeldPadSelection::new();
    let mut next_scan = Instant::now();
    let mut encoder_acceleration = EncoderAcceleration::new();

    loop {
        if let Ok(direction) = with_deadline(next_scan, encoder.read()).await {
            let ui_state = ui.lock(|state| *state.borrow());
            let target =
                EncoderTarget::for_controls(ui_state.selected_pad, ui_state.encoder_pressed);
            let direction_delta = match direction {
                Direction::Clockwise => 1,
                Direction::CounterClockwise => -1,
            };
            let delta =
                encoder_acceleration.update(Instant::now().as_millis(), target, direction_delta);
            shared.lock(|state| {
                let mut state = state.borrow_mut();
                apply_encoder_delta(&mut state, target, delta);
            });
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
            encoder_button_debouncer.update(u16::from(encoder_button.is_low()));
            let encoder_pressed = encoder_button_debouncer.stable_mask() & 1 != 0;
            ui.lock(|state| {
                let mut state = state.borrow_mut();
                state.selected_pad = selection.selected();
                state.encoder_pressed = encoder_pressed;
            });

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
    mut pixels: PioWs2812<'static, PIO1, 0, PAD_COUNT, Grb>,
    mut status_led: Output<'static>,
    shared: &'static Shared,
    ui: &'static UiShared,
) {
    let mut ticker = Ticker::every(LED_INTERVAL);
    loop {
        let (playback_frame, triggers, underruns, brightness) = shared.lock(|state| {
            let state = state.borrow();
            (
                state.playback_frame,
                state.latest_trigger_frames,
                state.underrun_count,
                state.led_brightness_percent,
            )
        });
        let brightness_preview = ui.lock(|state| {
            let state = state.borrow();
            state.selected_pad.is_none() && state.encoder_pressed
        });

        let mut colors = [RGB8::default(); PAD_COUNT];
        for pad in 0..PAD_COUNT {
            if brightness_preview
                || led_pulse_active(playback_frame, triggers[pad], SAMPLE_RATE / 10)
            {
                let (r, g, b) = scale_color(colorwheel((21 * pad) as u8), brightness);
                colors[pad] = RGB8 { r, g, b };
            }
        }
        pixels.write(&colors).await;

        if underruns != 0 || FATAL_FAULT.load(Ordering::Relaxed) {
            status_led.set_high();
        }
        ticker.next().await;
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

    let mut last_value: Option<(EncoderTarget, u32)> = None;
    let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let mut ticker = Ticker::every(DISPLAY_INTERVAL);

    loop {
        let ui_state = ui.lock(|state| *state.borrow());
        let target = EncoderTarget::for_controls(ui_state.selected_pad, ui_state.encoder_pressed);
        let displayed_value = shared.lock(|state| {
            let state = state.borrow();
            match target {
                EncoderTarget::BaseInterval => state.base_interval_ms,
                EncoderTarget::LedBrightness => u32::from(state.led_brightness_percent),
                EncoderTarget::Pad(pad) => u32::from(state.desired_beats[pad]),
            }
        });
        let value = (target, displayed_value);

        if last_value != Some(value) {
            display.clear();
            let _ = Text::with_baseline("LoopTic", Point::new(0, 0), style, Baseline::Top)
                .draw(&mut display);

            let mut line: String<24> = String::new();
            match target {
                EncoderTarget::Pad(_) => {
                    let _ = write!(&mut line, "Beat {}", displayed_value);
                }
                EncoderTarget::BaseInterval => {
                    let _ = write!(&mut line, "Base {} ms", displayed_value);
                }
                EncoderTarget::LedBrightness => {
                    let _ = write!(&mut line, "Light {}%", displayed_value);
                }
            }
            let _ = Text::with_baseline(&line, Point::new(0, 16), style, Baseline::Top)
                .draw(&mut display);

            if display.flush().is_err() {
                FATAL_FAULT.store(true, Ordering::Relaxed);
            }
            last_value = Some(value);
        }

        ticker.next().await;
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

    let (kick, open_hat) = match (WavPcm16::parse(KICK_WAV), WavPcm16::parse(OPEN_HAT_WAV)) {
        (Ok(kick), Ok(open_hat)) => (kick, open_hat),
        _ => {
            // A steady low level is silent through the SynthPlug's AC-coupled output.
            let silent_audio = Output::new(p.PIN_20, Level::Low);
            fatal(&mut status_led, silent_audio)
        }
    };
    let sequencer = Sequencer::new(kick, open_hat);

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
