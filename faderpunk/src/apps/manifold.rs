use embassy_futures::{
    join::{join, join5},
    select::{select, select3},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use heapless::Vec;
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue,
    latch::LatchLayer,
    utils::{
        attenuverter, clickless, division_at, lfo_step, quant_step, signal_brightness, slew_lin,
        split_unsigned_value, SlewState,
    },
    AppIcon, Brightness, ClockDivision, Color, Config, Curve, Param, Range, Value, APP_MAX_PARAMS,
};

use crate::{
    app::{
        App, AppParams, AppStorage, ClockEvent, GateJack, Led, Leds, ManagedStorage, OutJack,
        ParamStore, SceneEvent,
    },
    apps::morph::{morph_sample, MorphChaos},
    tasks::leds::LedMode,
};

pub const CHANNELS: usize = 4;
pub const PARAMS: usize = 10;

const TRIG_HIGH: u16 = 2458;
const FADER_MOVE_THRESH: u16 = 64;
const BUTTON_BRIGHTNESS: Brightness = Brightness::Mid;
/// Idle button presence so the four channels stay identifiable as Manifold.
const BUTTON_IDLE_BRIGHTNESS: Brightness = Brightness::Low;
/// Input samples within this 12-bit distance count as unchanged (ADC noise floor).
const IN_DEADBAND: u16 = 24;
/// Milliseconds of unchanged input before `Source::Auto` falls back to the
/// internal LFO. Long enough that a slow CV ramp is never mistaken for silence.
const IN_IDLE_MS: u16 = 1200;
/// Hold off periodic button LED writes so LedMode::Flash can finish.
const BUTTON_FLASH_MS: u16 = 850;
/// Internal LFO shape defaults for the axes Manifold does not expose:
/// balanced halves, no time warp. Skew stays on the Ch0 Third layer.
const LFO_SYMMETRY: u16 = 2048;
const LFO_WARP: u16 = 0;
/// Clock divisions the speed fader spans (slowest 384 ticks … 6 = quarter note).
const LFO_DIVISIONS: usize = 9;

const fn pulse_ms_to_fader(ms: u16) -> u16 {
    let ms = if ms < 1 {
        1
    } else if ms > 100 {
        100
    } else {
        ms
    };
    ((ms - 1) as u32 * 4095 / 99) as u16
}

const fn pulse_fader_to_ms(value: u16) -> u16 {
    1 + (value as u32 * 99 / 4095) as u16
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Cv,
    Gate,
    Trigger,
}

impl Mode {
    fn from_usize(v: usize) -> Self {
        match v {
            1 => Mode::Gate,
            2 => Mode::Trigger,
            _ => Mode::Cv,
        }
    }

    /// Button / meter colour for this out type. CV keeps the app colour;
    /// Gate and Trigger use fixed hues so the three outs read at a glance.
    fn color(self, app_color: Color) -> Color {
        match self {
            Mode::Cv => app_color,
            Mode::Gate => Color::Yellow,
            Mode::Trigger => Color::Orange,
        }
    }
}

/// Where the three outs take their signal from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Internal LFO whenever the input jack has been static long enough.
    Auto,
    /// Always the input jack, even when it holds a steady DC level.
    CvIn,
    /// Always the internal LFO; the input jack is ignored.
    Lfo,
}

impl Source {
    fn from_usize(v: usize) -> Self {
        match v {
            1 => Source::CvIn,
            2 => Source::Lfo,
            _ => Source::Auto,
        }
    }
}

enum OutJackType {
    Cv(OutJack),
    Gate(GateJack),
}

fn paint_bipolar_level(leds: &Leds<CHANNELS>, chan: usize, color: Color, level: u16) {
    let parts = split_unsigned_value(level);
    leds.set(chan, Led::Top, color, Brightness::Custom(parts[0]));
    leds.set(chan, Led::Bottom, color, Brightness::Custom(parts[1]));
}

fn paint_buttons(
    leds: &Leds<CHANNELS>,
    in_color: Color,
    out_colors: [Color; 3],
    frozen: bool,
    muted: [bool; 3],
) {
    leds.set(
        0,
        Led::Button,
        in_color,
        if frozen {
            BUTTON_BRIGHTNESS
        } else {
            BUTTON_IDLE_BRIGHTNESS
        },
    );
    for (i, &muted_i) in muted.iter().enumerate() {
        if muted_i {
            leds.unset(i + 1, Led::Button);
        } else {
            leds.set(i + 1, Led::Button, out_colors[i], BUTTON_BRIGHTNESS);
        }
    }
}

pub static CONFIG: Config<PARAMS> = Config::new(
    "Manifold",
    "One CV in, three shaped outs with comparator modes",
    Color::Violet,
    AppIcon::Attenuate,
)
.add_param(Param::Color {
    name: "Color",
    variants: &[
        Color::Blue,
        Color::Green,
        Color::Rose,
        Color::Orange,
        Color::Cyan,
        Color::Pink,
        Color::Violet,
        Color::Yellow,
    ],
})
.add_param(Param::Range {
    name: "In Range",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Enum {
    name: "Mode B",
    variants: &["CV", "Gate", "Trigger"],
})
.add_param(Param::Range {
    name: "Range B",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Enum {
    name: "Mode C",
    variants: &["CV", "Gate", "Trigger"],
})
.add_param(Param::Range {
    name: "Range C",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Enum {
    name: "Mode D",
    variants: &["CV", "Gate", "Trigger"],
})
.add_param(Param::Range {
    name: "Range D",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Enum {
    name: "Source",
    variants: &["Auto", "CV In", "Internal LFO"],
})
.add_param(Param::Enum {
    name: "LFO Speed",
    variants: &["Normal", "Slow", "Slowest"],
});

pub struct Params {
    color: Color,
    in_range: Range,
    mode_b: Mode,
    range_b: Range,
    mode_c: Mode,
    range_c: Range,
    mode_d: Mode,
    range_d: Range,
    source: Source,
    lfo_speed_mult: usize,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        // Legacy: 8 params (no internal LFO), 10 = current.
        if values.len() < 8 {
            return None;
        }
        Some(Self {
            color: Color::from_value(values[0]),
            in_range: Range::from_value(values[1]),
            mode_b: Mode::from_usize(usize::from_value(values[2])),
            range_b: Range::from_value(values[3]),
            mode_c: Mode::from_usize(usize::from_value(values[4])),
            range_c: Range::from_value(values[5]),
            mode_d: Mode::from_usize(usize::from_value(values[6])),
            range_d: Range::from_value(values[7]),
            source: if values.len() >= 9 {
                Source::from_usize(usize::from_value(values[8]))
            } else {
                Source::Auto
            },
            lfo_speed_mult: if values.len() >= 10 {
                usize::from_value(values[9])
            } else {
                0
            },
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.color.into()).unwrap();
        vec.push(self.in_range.into()).unwrap();
        vec.push(Value::Enum(self.mode_b as usize)).unwrap();
        vec.push(self.range_b.into()).unwrap();
        vec.push(Value::Enum(self.mode_c as usize)).unwrap();
        vec.push(self.range_c.into()).unwrap();
        vec.push(Value::Enum(self.mode_d as usize)).unwrap();
        vec.push(self.range_d.into()).unwrap();
        vec.push(Value::Enum(self.source as usize)).unwrap();
        vec.push(Value::Enum(self.lfo_speed_mult)).unwrap();
        vec
    }
}

#[derive(Serialize, Deserialize)]
pub struct Storage {
    in_trim: u16,
    in_offset: u16,
    in_slew: u16,
    att: [u16; 3],
    offset: [u16; 3],
    slew: [u16; 3],
    threshold: [u16; 3],
    hysteresis: [u16; 3],
    pulse: [u16; 3],
    muted: [bool; 3],
    /// Internal-LFO layers on Ch0 (Main / Alt / Third) plus its clock sync.
    morph: u16,
    lfo_speed: u16,
    skew: u16,
    lfo_clocked: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            in_trim: 4095,
            in_offset: 2047,
            in_slew: 0,
            att: [4095; 3],
            offset: [2047; 3],
            slew: [0; 3],
            threshold: [TRIG_HIGH; 3],
            hysteresis: [100; 3],
            pulse: [pulse_ms_to_fader(10); 3],
            muted: [false; 3],
            morph: 0,
            lfo_speed: 2000,
            skew: 2048,
            lfo_clocked: false,
        }
    }
}

impl AppStorage for Storage {}

#[embassy_executor::task(pool_size = 16/CHANNELS)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(
        app.app_id,
        app.layout_id,
        Params {
            color: Color::Violet,
            in_range: Range::_Neg5_5V,
            mode_b: Mode::Cv,
            range_b: Range::_Neg5_5V,
            mode_c: Mode::Gate,
            range_c: Range::_Neg5_5V,
            mode_d: Mode::Trigger,
            range_d: Range::_Neg5_5V,
            source: Source::Auto,
            lfo_speed_mult: 0,
        },
    );
    let storage = ManagedStorage::<Storage>::new(app.app_id, app.layout_id);

    param_store.load().await;
    storage.load().await;

    let app_loop = async {
        loop {
            select3(
                run(&app, &param_store, &storage),
                param_store.param_handler(),
                storage.saver_task(),
            )
            .await;
        }
    };

    select(app_loop, app.exit_handler(exit_signal)).await;
}

async fn make_jack_for_mode(
    app: &App<CHANNELS>,
    chan: usize,
    mode: Mode,
    range: Range,
) -> OutJackType {
    match mode {
        Mode::Cv => OutJackType::Cv(app.make_out_jack(chan, range).await),
        Mode::Gate | Mode::Trigger => {
            let g = app.make_gate_jack(chan, 4095).await;
            // make_gate_jack drives the port high on configure; force a known-off state.
            g.set_low().await;
            OutJackType::Gate(g)
        }
    }
}

pub async fn run(
    app: &App<CHANNELS>,
    params: &ParamStore<Params>,
    storage: &ManagedStorage<Storage>,
) {
    let (led_color, in_range, mode_b, range_b, mode_c, range_c, mode_d, range_d) =
        params.query(|p| {
            (
                p.color, p.in_range, p.mode_b, p.range_b, p.mode_c, p.range_c, p.mode_d, p.range_d,
            )
        });
    let source = params.query(|p| p.source);
    let lfo_speed_mult = 2u32.pow(params.query(|p| p.lfo_speed_mult).min(31) as u32);

    let modes = [mode_b, mode_c, mode_d];
    let ranges = [range_b, range_c, range_d];
    let out_colors = [
        modes[0].color(led_color),
        modes[1].color(led_color),
        modes[2].color(led_color),
    ];

    let buttons = app.use_buttons();
    let faders = app.use_faders();
    let leds = app.use_leds();

    let glob_in_trim = app.make_global(4095u16);
    let glob_in_offset = app.make_global(2047u16);
    let glob_in_slew = app.make_global(0u16);
    let glob_att = app.make_global([4095u16; 3]);
    let glob_offset = app.make_global([2047u16; 3]);
    let glob_slew = app.make_global([0u16; 3]);
    let glob_threshold = app.make_global([TRIG_HIGH; 3]);
    let glob_hysteresis = app.make_global([100u16; 3]);
    let glob_pulse = app.make_global([pulse_ms_to_fader(10); 3]);
    let glob_muted = app.make_global([false; 3]);
    let glob_frozen = app.make_global(false);

    let glob_fader_at_down = app.make_global([0u16; 4]);
    let glob_fader_moved = app.make_global([false; 4]);

    // Internal LFO: runs whenever `Source` resolves to it, so an unpatched
    // Manifold still has something to shape.
    let die = app.use_die();
    let mut clk = app.use_clock();
    let ticker = clk.get_ticker();
    let glob_lfo_active = app.make_global(source == Source::Lfo);
    let glob_lfo_pos = app.make_global(0.0f32);
    let glob_lfo_step = app.make_global(0.0682f32);
    let glob_quant_step = app.make_global(0.07f32);
    let glob_div = app.make_global(24u32);
    let glob_count = app.make_global(20u32);
    let glob_tick = app.make_global(false);
    let glob_phase_origin = app.make_global(0u64);
    // Transport hold while clocked: Stop parks the wave, Start/Tick releases it.
    let glob_clock_held = app.make_global(false);
    let glob_chaos = app.make_global(MorphChaos::new());
    let glob_btn_flash_0 = app.make_global(0u16);

    // Free-run step and clock division for the stored speed fader.
    let time_calc = || {
        let speed = storage.query(|s| s.lfo_speed);
        glob_lfo_step.set(lfo_step(speed));
        let div = division_at(speed, LFO_DIVISIONS);
        if div != glob_div.get() {
            glob_div.set(div);
            glob_phase_origin.set(0);
        }
        glob_quant_step.set(quant_step(glob_count.get(), div));
    };
    time_calc();

    let (in_trim, in_offset, in_slew, att, offset, slew, threshold, hysteresis, pulse, muted) =
        storage.query(|s| {
            (
                s.in_trim,
                s.in_offset,
                s.in_slew,
                s.att,
                s.offset,
                s.slew,
                s.threshold,
                s.hysteresis,
                s.pulse,
                s.muted,
            )
        });

    glob_in_trim.set(in_trim);
    glob_in_offset.set(in_offset);
    glob_in_slew.set(in_slew);
    glob_att.set(att);
    glob_offset.set(offset);
    glob_slew.set(slew);
    glob_threshold.set(threshold);
    glob_hysteresis.set(hysteresis);
    glob_pulse.set(pulse);
    glob_muted.set(muted);

    let in_jack = app.make_in_jack(0, in_range).await;
    let out_jacks = [
        make_jack_for_mode(app, 1, modes[0], ranges[0]).await,
        make_jack_for_mode(app, 2, modes[1], ranges[1]).await,
        make_jack_for_mode(app, 3, modes[2], ranges[2]).await,
    ];

    paint_buttons(&leds, led_color, out_colors, false, muted);

    let fut1 = async {
        let mut in_slew_state = SlewState::new();
        let mut out_slew_states = [SlewState::new(); 3];
        let mut gate_high = [false; 3];
        let mut pulse_left = [0u16; 3];
        let mut out_levels = [0u16; 3];
        let mut frozen = false;
        let mut frozen_value = 0u16;

        // Seeded from storage: a freshly spawned app sits on its saved values
        // instead of audibly gliding to them from the defaults.
        let mut prev_in_trim = in_trim;
        let mut prev_in_offset = in_offset;
        let mut prev_att = att;
        let mut prev_offset = offset;
        let mut prev_threshold = threshold;
        let mut prev_hysteresis = hysteresis;

        let mut prev_raw_input = in_jack.get_value();
        // Starts idle so an unpatched Manifold comes up on the internal LFO
        // instead of waiting out the timeout on a dead jack.
        let mut in_idle_ms = IN_IDLE_MS;
        let mut tick_count = 0u32;

        loop {
            app.delay_millis(1).await;

            let raw_input = in_jack.get_value();

            // Patch heuristic: the jack has no sense pin, so a live cable is
            // inferred from movement. Deadband covers the ADC noise floor.
            if raw_input.abs_diff(prev_raw_input) > IN_DEADBAND {
                in_idle_ms = 0;
            } else {
                in_idle_ms = in_idle_ms.saturating_add(1).min(IN_IDLE_MS);
            }
            prev_raw_input = raw_input;

            let lfo_active = match source {
                Source::CvIn => false,
                Source::Lfo => true,
                Source::Auto => in_idle_ms >= IN_IDLE_MS,
            };
            glob_lfo_active.set(lfo_active);

            let flash_0 = glob_btn_flash_0.get();
            if flash_0 > 0 {
                glob_btn_flash_0.set(flash_0.saturating_sub(1));
            }

            let (morph, skew, lfo_clocked) =
                storage.query(|s| (s.morph, s.skew, s.lfo_clocked));

            let is_frozen = glob_frozen.get();
            let clock_held = lfo_active && lfo_clocked && glob_clock_held.get();
            let held = is_frozen || clock_held;

            let source_val = if lfo_active {
                time_calc();

                tick_count = tick_count.saturating_add(1);
                if glob_tick.get() {
                    glob_count.set(tick_count);
                    tick_count = 0;
                    glob_tick.set(false);
                }

                let step = if lfo_clocked {
                    glob_quant_step.get()
                } else {
                    glob_lfo_step.get()
                } / lfo_speed_mult as f32;

                let pos = glob_lfo_pos.get();
                let next_pos = if held { pos } else { (pos + step) % 4096.0 };

                let mut chaos = glob_chaos.get();
                chaos.tick_walks(&die);
                let sample = morph_sample(
                    next_pos as usize,
                    morph,
                    (skew, LFO_WARP, LFO_SYMMETRY),
                    0,
                    &mut chaos,
                    &die,
                );
                glob_chaos.set(chaos);

                if !held {
                    glob_lfo_pos.set(next_pos);
                }
                sample
            } else {
                prev_in_trim = clickless(prev_in_trim, glob_in_trim.get());
                let trimmed = attenuverter(raw_input, Curve::Deadzone.at(prev_in_trim));

                prev_in_offset = clickless(prev_in_offset, glob_in_offset.get());
                let in_offset = Curve::Deadzone.at(prev_in_offset) as i32 - 2047;

                let conditioned_raw = (trimmed as i32 + in_offset).clamp(0, 4095) as u16;

                let in_slew_rate = glob_in_slew.get();
                in_slew_state = slew_lin(in_slew_state, conditioned_raw, in_slew_rate, in_slew_rate);
                in_slew_state.value()
            };

            // Freeze snapshots the value so stepped morph nodes hold too, not
            // just the phase.
            if held != frozen {
                frozen = held;
                if frozen {
                    frozen_value = source_val;
                }
            }
            let input_val = if frozen { frozen_value } else { source_val };

            // Channel 0: active source amplitude.
            paint_bipolar_level(&leds, 0, led_color, input_val);

            let muted = glob_muted.get();
            for i in 0..3 {
                let out_color = out_colors[i];
                let out_level = match modes[i] {
                    Mode::Cv => {
                        let att_arr = glob_att.get();
                        let offset_arr = glob_offset.get();
                        let slew_arr = glob_slew.get();

                        prev_att[i] = clickless(prev_att[i], att_arr[i]);
                        prev_offset[i] = clickless(prev_offset[i], offset_arr[i]);

                        let out_target = if muted[i] {
                            Curve::Deadzone.at(prev_offset[i])
                        } else {
                            let offset_signed = Curve::Deadzone.at(prev_offset[i]) as i32 - 2047;
                            (attenuverter(input_val, Curve::Deadzone.at(prev_att[i])) as i32
                                + offset_signed)
                                .clamp(0, 4095) as u16
                        };

                        let slew_rate = slew_arr[i];
                        out_slew_states[i] =
                            slew_lin(out_slew_states[i], out_target, slew_rate, slew_rate);
                        let out_level = out_slew_states[i].value();
                        if let OutJackType::Cv(j) = &out_jacks[i] {
                            j.set_value(out_level);
                        }
                        out_level
                    }
                    Mode::Gate => {
                        let threshold_arr = glob_threshold.get();
                        let hysteresis_arr = glob_hysteresis.get();

                        prev_threshold[i] = clickless(prev_threshold[i], threshold_arr[i]);
                        prev_hysteresis[i] = clickless(prev_hysteresis[i], hysteresis_arr[i]);

                        let threshold = prev_threshold[i];
                        let hysteresis = prev_hysteresis[i];
                        let min_gate_ms = pulse_fader_to_ms(glob_pulse.get()[i]);

                        if muted[i] {
                            if gate_high[i] {
                                gate_high[i] = false;
                                pulse_left[i] = 0;
                                if let OutJackType::Gate(g) = &out_jacks[i] {
                                    g.set_low().await;
                                }
                            }
                        } else if !gate_high[i] && input_val > threshold {
                            gate_high[i] = true;
                            pulse_left[i] = min_gate_ms;
                            if let OutJackType::Gate(g) = &out_jacks[i] {
                                g.set_high().await;
                            }
                        } else if gate_high[i]
                            && pulse_left[i] == 0
                            && input_val <= threshold.saturating_sub(hysteresis)
                        {
                            gate_high[i] = false;
                            if let OutJackType::Gate(g) = &out_jacks[i] {
                                g.set_low().await;
                            }
                        }

                        // Holds the gate up for the minimum length even if the input
                        // dips straight back below the threshold, so a fast crossing
                        // still produces a gate the receiving module can see.
                        pulse_left[i] = pulse_left[i].saturating_sub(1);

                        if gate_high[i] {
                            4095
                        } else {
                            0
                        }
                    }
                    Mode::Trigger => {
                        let threshold_arr = glob_threshold.get();
                        let hysteresis_arr = glob_hysteresis.get();
                        let pulse_arr = glob_pulse.get();

                        prev_threshold[i] = clickless(prev_threshold[i], threshold_arr[i]);
                        prev_hysteresis[i] = clickless(prev_hysteresis[i], hysteresis_arr[i]);

                        let threshold = prev_threshold[i];
                        let hysteresis = prev_hysteresis[i];
                        let pulse_ms = pulse_fader_to_ms(pulse_arr[i]);

                        if muted[i] {
                            if pulse_left[i] > 0 {
                                pulse_left[i] = 0;
                                if let OutJackType::Gate(g) = &out_jacks[i] {
                                    g.set_low().await;
                                }
                            }
                            gate_high[i] = false;
                        } else {
                            // Schmitt trigger: a slow ramp crossing the threshold
                            // must not emit a burst of triggers on input noise.
                            let high = if gate_high[i] {
                                input_val > threshold.saturating_sub(hysteresis)
                            } else {
                                input_val > threshold
                            };
                            if !gate_high[i] && high {
                                pulse_left[i] = pulse_ms;
                                if let OutJackType::Gate(g) = &out_jacks[i] {
                                    g.set_high().await;
                                }
                            }
                            gate_high[i] = high;
                        }

                        if pulse_left[i] > 0 {
                            pulse_left[i] -= 1;
                            if pulse_left[i] == 0 {
                                if let OutJackType::Gate(g) = &out_jacks[i] {
                                    g.set_low().await;
                                }
                            }
                        }

                        if pulse_left[i] > 0 {
                            4095
                        } else {
                            0
                        }
                    }
                };
                out_levels[i] = out_level;

                // Meter colour = out type. Dim = amplitude of that channel's
                // signal: CV uses the shaped out; Gate/Trigger flash full when
                // high and otherwise meter the conditioned input so amplitude
                // still reads while waiting for a threshold crossing.
                match modes[i] {
                    Mode::Cv => {
                        paint_bipolar_level(&leds, i + 1, out_color, out_level);
                    }
                    Mode::Gate | Mode::Trigger => {
                        if muted[i] {
                            leds.unset(i + 1, Led::Top);
                            leds.unset(i + 1, Led::Bottom);
                        } else if out_level > 0 {
                            leds.set(i + 1, Led::Top, out_color, Brightness::High);
                            leds.set(i + 1, Led::Bottom, out_color, Brightness::High);
                        } else {
                            paint_bipolar_level(&leds, i + 1, out_color, input_val);
                        }
                    }
                }
            }

            if flash_0 == 0 {
                leds.set(
                    0,
                    Led::Button,
                    led_color,
                    if held {
                        BUTTON_BRIGHTNESS
                    } else {
                        // The LFO always swings around mid-scale, regardless of
                        // the configured input range.
                        signal_brightness(input_val, lfo_active || in_range.is_bipolar())
                    },
                );
            }
            for i in 0..3 {
                if muted[i] {
                    leds.unset(i + 1, Led::Button);
                } else {
                    leds.set(
                        i + 1,
                        Led::Button,
                        out_colors[i],
                        signal_brightness(
                            out_levels[i],
                            modes[i] == Mode::Cv && ranges[i].is_bipolar(),
                        ),
                    );
                }
            }
        }
    };

    let fut2 = async {
        let mut latch = [
            app.make_latch(faders.get_value_at(0)),
            app.make_latch(faders.get_value_at(1)),
            app.make_latch(faders.get_value_at(2)),
            app.make_latch(faders.get_value_at(3)),
        ];

        loop {
            let chan = faders.wait_for_any_change().await;
            let fader_val = faders.get_value_at(chan);

            if buttons.is_button_pressed(chan) && !buttons.is_shift_pressed() {
                let at_down = glob_fader_at_down.get()[chan];
                let delta = fader_val.abs_diff(at_down);
                if delta > FADER_MOVE_THRESH {
                    glob_fader_moved.modify(|m| {
                        let mut arr = *m;
                        arr[chan] = true;
                        arr
                    });
                }
            }

            let latch_active_layer =
                if buttons.is_shift_pressed() && !buttons.is_button_pressed(chan) {
                    LatchLayer::Alt
                } else if !buttons.is_shift_pressed() && buttons.is_button_pressed(chan) {
                    LatchLayer::Third
                } else {
                    LatchLayer::Main
                };

            // Ch0 layers follow the active source: with no cable the input
            // trim / offset / slew are meaningless, so the LFO takes them over.
            let lfo_active = glob_lfo_active.get();

            let target_value = if chan == 0 {
                match (lfo_active, latch_active_layer) {
                    (true, LatchLayer::Main) => storage.query(|s| s.morph),
                    (true, LatchLayer::Alt) => storage.query(|s| s.lfo_speed),
                    (true, LatchLayer::Third) => storage.query(|s| s.skew),
                    (false, LatchLayer::Main) => storage.query(|s| s.in_trim),
                    (false, LatchLayer::Alt) => storage.query(|s| s.in_offset),
                    (false, LatchLayer::Third) => storage.query(|s| s.in_slew),
                }
            } else {
                let i = chan - 1;
                match modes[i] {
                    Mode::Cv => match latch_active_layer {
                        LatchLayer::Main => storage.query(|s| s.att[i]),
                        LatchLayer::Alt => storage.query(|s| s.offset[i]),
                        LatchLayer::Third => storage.query(|s| s.slew[i]),
                    },
                    Mode::Gate | Mode::Trigger => match latch_active_layer {
                        LatchLayer::Main => storage.query(|s| s.threshold[i]),
                        LatchLayer::Alt => storage.query(|s| s.hysteresis[i]),
                        LatchLayer::Third => storage.query(|s| s.pulse[i]),
                    },
                }
            };

            if let Some(new_value) = latch[chan].update(fader_val, latch_active_layer, target_value)
            {
                if chan == 0 {
                    match (lfo_active, latch_active_layer) {
                        (true, LatchLayer::Main) => {
                            storage.modify_and_save(|s| s.morph = new_value);
                        }
                        (true, LatchLayer::Alt) => {
                            storage.modify_and_save(|s| s.lfo_speed = new_value);
                            time_calc();
                        }
                        (true, LatchLayer::Third) => {
                            storage.modify_and_save(|s| s.skew = new_value);
                        }
                        (false, LatchLayer::Main) => {
                            storage.modify_and_save(|s| s.in_trim = new_value);
                            glob_in_trim.set(new_value);
                        }
                        (false, LatchLayer::Alt) => {
                            storage.modify_and_save(|s| s.in_offset = new_value);
                            glob_in_offset.set(new_value);
                        }
                        (false, LatchLayer::Third) => {
                            storage.modify_and_save(|s| s.in_slew = new_value);
                            glob_in_slew.set(new_value);
                        }
                    }
                } else {
                    let i = chan - 1;
                    match modes[i] {
                        Mode::Cv => match latch_active_layer {
                            LatchLayer::Main => {
                                storage.modify_and_save(|s| s.att[i] = new_value);
                                glob_att.modify(|a| {
                                    let mut arr = *a;
                                    arr[i] = new_value;
                                    arr
                                });
                            }
                            LatchLayer::Alt => {
                                storage.modify_and_save(|s| s.offset[i] = new_value);
                                glob_offset.modify(|o| {
                                    let mut arr = *o;
                                    arr[i] = new_value;
                                    arr
                                });
                            }
                            LatchLayer::Third => {
                                storage.modify_and_save(|s| s.slew[i] = new_value);
                                glob_slew.modify(|s| {
                                    let mut arr = *s;
                                    arr[i] = new_value;
                                    arr
                                });
                            }
                        },
                        Mode::Gate | Mode::Trigger => match latch_active_layer {
                            LatchLayer::Main => {
                                storage.modify_and_save(|s| s.threshold[i] = new_value);
                                glob_threshold.modify(|t| {
                                    let mut arr = *t;
                                    arr[i] = new_value;
                                    arr
                                });
                            }
                            LatchLayer::Alt => {
                                storage.modify_and_save(|s| s.hysteresis[i] = new_value);
                                glob_hysteresis.modify(|h| {
                                    let mut arr = *h;
                                    arr[i] = new_value;
                                    arr
                                });
                            }
                            LatchLayer::Third => {
                                storage.modify_and_save(|s| s.pulse[i] = new_value);
                                glob_pulse.modify(|p| {
                                    let mut arr = *p;
                                    arr[i] = new_value;
                                    arr
                                });
                            }
                        },
                    }
                }
            }
        }
    };

    // Down and up run as separate loops: every `wait_for_*` call opens its own
    // subscriber, so waiting for one channel's release inside the down handler
    // would swallow every other channel's events while a button is held.
    let button_down = async {
        loop {
            let (chan, _) = buttons.wait_for_any_down().await;

            glob_fader_at_down.modify(|a| {
                let mut arr = *a;
                arr[chan] = faders.get_value_at(chan);
                arr
            });
            glob_fader_moved.modify(|m| {
                let mut arr = *m;
                arr[chan] = false;
                arr
            });
        }
    };

    let button_up = async {
        loop {
            let (chan, shift) = buttons.wait_for_any_up().await;

            if glob_fader_moved.get()[chan] {
                continue;
            }

            match chan {
                0 if shift && glob_lfo_active.get() => {
                    let clocked = storage.modify_and_save(|s| {
                        s.lfo_clocked = !s.lfo_clocked;
                        s.lfo_clocked
                    });
                    if clocked {
                        // Already stopped when engaging sync → hold immediately.
                        if !crate::state::is_clock_running().await {
                            glob_clock_held.set(true);
                        }
                        leds.set_mode(0, Led::Button, LedMode::Flash(led_color, Some(4)));
                        glob_btn_flash_0.set(BUTTON_FLASH_MS);
                    } else {
                        glob_clock_held.set(false);
                    }
                }
                0 => {
                    let frozen = glob_frozen.toggle();
                    paint_buttons(&leds, led_color, out_colors, frozen, glob_muted.get());
                }
                1..=3 => {
                    let i = chan - 1;
                    let muted = storage.modify_and_save(|s| {
                        s.muted[i] = !s.muted[i];
                        s.muted[i]
                    });
                    let muted_all = glob_muted.modify(|m| {
                        let mut arr = *m;
                        arr[i] = muted;
                        arr
                    });
                    paint_buttons(&leds, led_color, out_colors, glob_frozen.get(), muted_all);
                }
                _ => {}
            }
        }
    };

    let fut3 = join(button_down, button_up);

    let clock_handler = async {
        loop {
            match clk.wait_for_event(ClockDivision::_1).await {
                ClockEvent::Tick(_) => {
                    let clocked = storage.query(|s| s.lfo_clocked);
                    if clocked {
                        glob_clock_held.set(false);
                        if !glob_frozen.get() {
                            // Phase lock: derive position from the tick counter so
                            // the wave cannot drift away from the transport.
                            let ticks_per_cycle =
                                (glob_div.get() as u64).saturating_mul(lfo_speed_mult as u64);
                            if ticks_per_cycle > 0 {
                                let phase_in_cycle =
                                    ticker().wrapping_sub(glob_phase_origin.get()) % ticks_per_cycle;
                                glob_lfo_pos
                                    .set(phase_in_cycle as f32 * 4096.0 / ticks_per_cycle as f32);
                            }
                        }
                    }
                    glob_tick.set(true);
                }
                ClockEvent::Start => {
                    if storage.query(|s| s.lfo_clocked) {
                        glob_clock_held.set(false);
                    }
                }
                ClockEvent::Stop => {
                    if storage.query(|s| s.lfo_clocked) {
                        glob_clock_held.set(true);
                    }
                }
                ClockEvent::Reset => {
                    glob_phase_origin.set(0);
                    glob_lfo_pos.set(0.0);
                }
            }
        }
    };

    let scene_handler = async {
        loop {
            match app.wait_for_scene_event().await {
                SceneEvent::LoadScene(scene) => {
                    storage.load_from_scene(scene).await;

                    let (
                        in_trim,
                        in_offset,
                        in_slew,
                        att,
                        offset,
                        slew,
                        threshold,
                        hysteresis,
                        pulse,
                        muted,
                    ) = storage.query(|s| {
                        (
                            s.in_trim,
                            s.in_offset,
                            s.in_slew,
                            s.att,
                            s.offset,
                            s.slew,
                            s.threshold,
                            s.hysteresis,
                            s.pulse,
                            s.muted,
                        )
                    });

                    glob_in_trim.set(in_trim);
                    glob_in_offset.set(in_offset);
                    glob_in_slew.set(in_slew);
                    glob_att.set(att);
                    glob_offset.set(offset);
                    glob_slew.set(slew);
                    glob_threshold.set(threshold);
                    glob_hysteresis.set(hysteresis);
                    glob_pulse.set(pulse);
                    glob_muted.set(muted);

                    time_calc();

                    paint_buttons(&leds, led_color, out_colors, glob_frozen.get(), muted);
                }
                SceneEvent::SaveScene(scene) => storage.save_to_scene(scene).await,
            }
        }
    };

    join5(fut1, fut2, fut3, scene_handler, clock_handler).await;
}
