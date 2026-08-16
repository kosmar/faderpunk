use embassy_futures::{
    join::{join, join3, join4},
    select::{select, select3},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use heapless::Vec;
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue,
    latch::LatchLayer,
    utils::{
        attenuate_bipolar, clickless, division_at, lfo_step, midi_gate, signal_brightness,
        split_unsigned_value,
    },
    AppIcon, Brightness, Color, Config, MidiCc, MidiChannel, MidiOut, Param, Range, Value,
    APP_MAX_PARAMS,
};

use crate::{
    app::{
        App, AppParams, AppStorage, Led, Leds, ManagedStorage, MidiOutput, ParamStore, SceneEvent,
    },
    apps::{
        led_fx::hsv_to_rgb,
        morph::{morph_sample, MorphChaos},
    },
    tasks::leds::LedMode,
};

pub const CHANNELS: usize = 4;
pub const PARAMS: usize = 15;

/// `Ch Map` packs one MIDI channel per wave into a nibble (wave 0 = bits 0..3,
/// nibble + 1 = channel 1..16). A whole map of 0 means "every wave follows the
/// base MIDI Channel", which is the shipped default.
const CH_MAP_FOLLOW: i32 = 0;
const CH_MAP_MAX: i32 = (1 << (4 * CHANNELS)) - 1;
/// `CC Map` packs one CC number per wave into 7 bits (wave 0 = bits 0..6). A
/// whole map of 0 means "every wave follows the base MIDI CC" via the
/// base + offset derivation, which is the shipped default.
const CC_MAP_FOLLOW: i32 = 0;
const CC_MAP_MAX: i32 = (1 << (7 * CHANNELS)) - 1;
/// Keeps the literal bounds in `CONFIG` honest against the packing above.
const _: () = assert!(CH_MAP_FOLLOW == 0 && CH_MAP_MAX == 65_535);
const _: () = assert!(CC_MAP_FOLLOW == 0 && CC_MAP_MAX == 268_435_455);

/// DSP loop period. 8 ms rather than 1 ms: a 1 kHz loop that also mirrors MIDI
/// starves the config SysEx path in dense layouts.
const AUDIO_MS: u16 = 8;
const FADER_MOVE_THRESH: u16 = 64;
const BUTTON_BRIGHTNESS: Brightness = Brightness::Mid;
const BUTTON_IDLE_BRIGHTNESS: Brightness = Brightness::Low;
/// Input samples within this 12-bit distance count as unchanged (ADC noise floor).
const IN_DEADBAND: u16 = 24;
/// Milliseconds of unchanged input before the root falls back to the internal
/// LFO.
const IN_IDLE_MS: u16 = 1200;
/// Hold off periodic button LED writes so LedMode::Flash can finish.
const BUTTON_FLASH_MS: u16 = 848;
/// One `LedMode::Flash` cycle ≈ 16 frames at 60 Hz.
const RANGE_FLASH_CYCLE_MS: u16 = 270;
/// Shape defaults for the axes that are not exposed per stage.
const LFO_WARP: u16 = 0;
const STAGE_SKEW: u16 = 2048;
/// Clock divisions the root speed fader spans.
const LFO_DIVISIONS: usize = 9;
/// Half-span of the exponential rate modulation, in octaves.
const RATE_MOD_OCTAVES: f32 = 3.0;
/// Ceiling for the per-iteration phase advance, in 1/4096 of a cycle.
///
/// The loop samples at 1000/AUDIO_MS Hz, so a step of 2048 sits exactly on
/// Nyquist and anything beyond it folds back as noise. Rate modulation can push
/// three octaves above a base rate that already reaches ~15 Hz, so it has to be
/// caught here. The limit is 1024 rather than 2048: four samples per cycle
/// still traces a recognisable wave, two draw a square no matter the morph.
const MAX_PHASE_STEP: f32 = 1024.0;
/// Five mode hues, same ~37.5 deg raster as Manifold: CV-in, LFO-in, then
/// mod destinations Rate / Depth / Shape. Started at 221-19 so matching mode
/// buttons (not channels) read apart from Manifold.
const RIPPPPLE_HUES: [u16; 5] = [202, 240, 277, 315, 352];

fn ripppple_color(step: usize) -> Color {
    let (r, g, b) = hsv_to_rgb(RIPPPPLE_HUES[step.min(4)] % 360);
    Color::Custom(r, g, b)
}

fn ch0_color(lfo_active: bool) -> Color {
    if lfo_active {
        ripppple_color(1)
    } else {
        ripppple_color(0)
    }
}

/// What the incoming stage signal modulates on this stage.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dest {
    Rate,
    Depth,
    Shape,
}

impl Dest {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Dest::Depth,
            2 => Dest::Shape,
            _ => Dest::Rate,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Dest::Rate => 0,
            Dest::Depth => 1,
            Dest::Shape => 2,
        }
    }

    fn next(self) -> Self {
        match self {
            Dest::Rate => Dest::Depth,
            Dest::Depth => Dest::Shape,
            Dest::Shape => Dest::Rate,
        }
    }

    fn color(self) -> Color {
        match self {
            Dest::Rate => ripppple_color(2),
            Dest::Depth => ripppple_color(3),
            Dest::Shape => ripppple_color(4),
        }
    }
}

fn next_range(range: Range) -> Range {
    match range {
        Range::_0_10V => Range::_Neg5_5V,
        _ => Range::_0_10V,
    }
}

/// ±5V → one blink; 0–10V → two blinks.
fn range_flash_times(range: Range) -> usize {
    if range.is_bipolar() {
        1
    } else {
        2
    }
}

fn range_flash_hold_ms(times: usize) -> u16 {
    RANGE_FLASH_CYCLE_MS
        .saturating_mul(times as u16)
        .saturating_add(40)
}

fn paint_bipolar_level(leds: &Leds<CHANNELS>, chan: usize, color: Color, level: u16) {
    let parts = split_unsigned_value(level);
    leds.set(chan, Led::Top, color, Brightness::Custom(parts[0]));
    leds.set(chan, Led::Bottom, color, Brightness::Custom(parts[1]));
}

fn paint_buttons(
    leds: &Leds<CHANNELS>,
    in_color: Color,
    dest: [u8; 3],
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
            leds.set(
                i + 1,
                Led::Button,
                Dest::from_u8(dest[i]).color(),
                BUTTON_BRIGHTNESS,
            );
        }
    }
}

pub static CONFIG: Config<PARAMS> = Config::new(
    "Ripppple",
    "One root LFO cascading through three modulated LFOs",
    Color::Cyan,
    AppIcon::Sine,
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
.add_param(Param::Range {
    name: "Range B",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Range {
    name: "Range C",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Range {
    name: "Range D",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Enum {
    name: "Target B",
    variants: &["Rate", "Depth", "Shape"],
})
.add_param(Param::Enum {
    name: "Target C",
    variants: &["Rate", "Depth", "Shape"],
})
.add_param(Param::Enum {
    name: "Target D",
    variants: &["Rate", "Depth", "Shape"],
})
.add_param(Param::Enum {
    name: "LFO Speed",
    variants: &["Normal", "Slow", "Slowest"],
})
.add_param(Param::MidiOut)
.add_param(Param::MidiChannel {
    name: "MIDI Channel",
})
.add_param(Param::MidiCc { name: "MIDI CC" })
.add_param(Param::MidiNrpn)
// Literal bounds: the catalog generator reads these as syntax, and a path
// expression would come out as an enum tag instead of a number.
.add_param(Param::i32 {
    name: "Ch Map",
    min: 0,
    max: 65_535,
})
.add_param(Param::i32 {
    name: "CC Map",
    min: 0,
    max: 268_435_455,
});

pub struct Params {
    color: Color,
    in_range: Range,
    range_b: Range,
    range_c: Range,
    range_d: Range,
    /// Start value of the per-stage modulation target (Ch1..Ch3); runtime state
    /// lives in `Storage::dest`.
    target: [usize; 3],
    lfo_speed_mult: usize,
    midi_out: MidiOut,
    midi_channel: MidiChannel,
    /// Base CC; the four channels take base + 0..=3.
    midi_cc: MidiCc,
    nrpn: bool,
    /// Four packed nibbles, one channel per wave. See `CH_MAP_FOLLOW`.
    ch_map: i32,
    /// Four packed 7-bit fields, one CC per wave. See `CC_MAP_FOLLOW`.
    cc_map: i32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            color: Color::Cyan,
            in_range: Range::_Neg5_5V,
            range_b: Range::_Neg5_5V,
            range_c: Range::_Neg5_5V,
            range_d: Range::_Neg5_5V,
            target: [0; 3],
            lfo_speed_mult: 0,
            midi_out: MidiOut([false; 3]),
            midi_channel: MidiChannel::default(),
            midi_cc: MidiCc::from(32u8),
            nrpn: false,
            ch_map: CH_MAP_FOLLOW,
            cc_map: CC_MAP_FOLLOW,
        }
    }
}

impl Params {
    /// Resolve the sending MIDI channel for one wave.
    fn channel_for(&self, wave: usize) -> MidiChannel {
        if self.ch_map == CH_MAP_FOLLOW {
            return self.midi_channel;
        }
        let nibble = ((self.ch_map >> (4 * wave)) & 0xF) as u8;
        MidiChannel::from(nibble + 1)
    }

    /// Resolve the CC number for one wave.
    fn cc_for(&self, wave: usize, nrpn: bool) -> MidiCc {
        if self.cc_map == CC_MAP_FOLLOW {
            return channel_cc(self.midi_cc, wave, nrpn);
        }
        let field = ((self.cc_map >> (7 * wave)) & 0x7F) as u16;
        MidiCc::from(field.min(midi_cc_limit(nrpn)))
    }
}

/// Highest CC number the transport can carry: NRPN uses the full 14-bit
/// parameter space, plain CC only 7 bit.
const fn midi_cc_limit(nrpn: bool) -> u16 {
    if nrpn {
        16383
    } else {
        127
    }
}

fn channel_cc(base: MidiCc, chan: usize, nrpn: bool) -> MidiCc {
    MidiCc::from(
        base.as_u16()
            .saturating_add(chan as u16)
            .min(midi_cc_limit(nrpn)),
    )
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        // Tolerant of short legacy slices: missing tail params fall back to the
        // silent / neutral defaults rather than panicking.
        if values.is_empty() {
            return None;
        }
        let at = |i: usize| values.get(i).copied();
        // Legacy layouts stored Source at index 8; skip it when the slice is
        // still 16-wide or when index 9 still holds the old LFO Speed enum.
        let legacy_source = values.len() >= 16
            || matches!(values.get(9), Some(Value::Enum(_)))
                && values.get(10).is_some();
        let tail = if legacy_source { 1 } else { 0 };
        Some(Self {
            color: Color::from_value(values[0]),
            in_range: at(1).map(Range::from_value).unwrap_or(Range::_Neg5_5V),
            range_b: at(2).map(Range::from_value).unwrap_or(Range::_Neg5_5V),
            range_c: at(3).map(Range::from_value).unwrap_or(Range::_Neg5_5V),
            range_d: at(4).map(Range::from_value).unwrap_or(Range::_Neg5_5V),
            target: [
                at(5).map(usize::from_value).unwrap_or(0).min(2),
                at(6).map(usize::from_value).unwrap_or(0).min(2),
                at(7).map(usize::from_value).unwrap_or(0).min(2),
            ],
            lfo_speed_mult: at(8 + tail).map(usize::from_value).unwrap_or(0),
            midi_out: at(9 + tail)
                .map(MidiOut::from_value)
                .unwrap_or(MidiOut([false; 3])),
            midi_channel: at(10 + tail)
                .map(MidiChannel::from_value)
                .unwrap_or_default(),
            midi_cc: at(11 + tail)
                .map(MidiCc::from_value)
                .unwrap_or(MidiCc::from(32u8)),
            nrpn: at(12 + tail).map(bool::from_value).unwrap_or(false),
            ch_map: at(13 + tail)
                .map(i32::from_value)
                .unwrap_or(CH_MAP_FOLLOW)
                .clamp(CH_MAP_FOLLOW, CH_MAP_MAX),
            cc_map: at(14 + tail)
                .map(i32::from_value)
                .unwrap_or(CC_MAP_FOLLOW)
                .clamp(CC_MAP_FOLLOW, CC_MAP_MAX),
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.color.into()).unwrap();
        vec.push(self.in_range.into()).unwrap();
        vec.push(self.range_b.into()).unwrap();
        vec.push(self.range_c.into()).unwrap();
        vec.push(self.range_d.into()).unwrap();
        for t in self.target {
            vec.push(Value::Enum(t)).unwrap();
        }
        vec.push(Value::Enum(self.lfo_speed_mult)).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.midi_channel.into()).unwrap();
        vec.push(self.midi_cc.into()).unwrap();
        vec.push(Value::MidiNrpn(self.nrpn)).unwrap();
        vec.push(self.ch_map.into()).unwrap();
        vec.push(self.cc_map.into()).unwrap();
        vec
    }
}

#[derive(Serialize, Deserialize)]
pub struct Storage {
    /// Per stage: modulation depth, base rate, own morph, modulation target.
    depth: [u16; 3],
    rate: [u16; 3],
    shape: [u16; 3],
    dest: [u8; 3],
    muted: [bool; 3],
    /// Root LFO layers (Main / Alt / Third) plus its clock sync.
    lfo_speed: u16,
    morph: u16,
    skew: u16,
    lfo_clocked: bool,
    /// Shared across all three stages (Ch3 Third layer).
    symmetry: u16,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            // Chain starts decoupled: every stage free-runs until depth is up.
            depth: [0; 3],
            rate: [2000; 3],
            shape: [0; 3],
            dest: [Dest::Rate.as_u8(); 3],
            muted: [false; 3],
            lfo_speed: 2000,
            morph: 0,
            skew: 2048,
            lfo_clocked: false,
            symmetry: 2048,
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
            color: Color::Cyan,
            in_range: Range::_Neg5_5V,
            range_b: Range::_Neg5_5V,
            range_c: Range::_Neg5_5V,
            range_d: Range::_Neg5_5V,
            target: [0; 3],
            lfo_speed_mult: 0,
            midi_out: MidiOut([false; 3]),
            midi_channel: MidiChannel::default(),
            midi_cc: MidiCc::from(32u8.saturating_add(app.start_channel as u8)),
            ..Default::default()
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

pub async fn run(
    app: &App<CHANNELS>,
    params: &ParamStore<Params>,
    storage: &ManagedStorage<Storage>,
) {
    let (in_range, range_b, range_c, range_d) =
        params.query(|p| (p.in_range, p.range_b, p.range_c, p.range_d));
    let (midi_out, nrpn) = params.query(|p| (p.midi_out, p.nrpn));
    let midi_chans = params.query(|p| core::array::from_fn::<_, CHANNELS, _>(|w| p.channel_for(w)));
    let midi_ccs =
        params.query(|p| core::array::from_fn::<_, CHANNELS, _>(|w| p.cc_for(w, p.nrpn)));
    let lfo_speed_mult = 2u32.pow(params.query(|p| p.lfo_speed_mult).min(31) as u32);

    // Configurator "Target B/C/D" are start values; applied once per run() (a
    // host param edit restarts run). A scene load overrides storage later.
    let p_target = params.query(|p| p.target);
    storage.modify_and_save(|s| {
        for (slot, t) in s.dest.iter_mut().zip(p_target.iter()) {
            *slot = (*t).min(2) as u8;
        }
    });

    let ranges = [range_b, range_c, range_d];
    let bipolar = [
        range_b.is_bipolar(),
        range_c.is_bipolar(),
        range_d.is_bipolar(),
    ];
    let initial_lfo_active = true;

    let midi: [MidiOutput; CHANNELS] =
        core::array::from_fn(|w| app.use_midi_output(midi_out, midi_chans[w], nrpn));

    let buttons = app.use_buttons();
    let faders = app.use_faders();
    let leds = app.use_leds();

    let glob_depth = app.make_global([0u16; 3]);
    let glob_rate = app.make_global([2000u16; 3]);
    let glob_shape = app.make_global([0u16; 3]);
    let glob_dest = app.make_global([0u8; 3]);
    let glob_muted = app.make_global([false; 3]);
    let glob_symmetry = app.make_global(2048u16);
    let glob_frozen = app.make_global(false);

    let glob_fader_at_down = app.make_global([0u16; 4]);
    let glob_fader_moved = app.make_global([false; 4]);
    let glob_long_press = app.make_global([false; 4]);

    // Signalled when an out range changes: run() has to return so the jack is
    // reconfigured.
    let restart = Signal::<NoopRawMutex, ()>::new();

    let die = app.use_die();
    // clock_ticker, never use_clock: an undrained CLOCK_PUBSUB subscriber
    // stalls the gatekeeper's blocking publish and kills the device clock.
    let ticker = app.clock_ticker();
    let glob_lfo_active = app.make_global(true);
    let glob_lfo_step = app.make_global(0.0682f32);
    let glob_div = app.make_global(24u32);
    let glob_btn_flash = app.make_global([0u16; 4]);

    let time_calc = || {
        let speed = storage.query(|s| s.lfo_speed);
        glob_lfo_step.set(lfo_step(speed));
        glob_div.set(division_at(speed, LFO_DIVISIONS));
    };
    time_calc();

    let (depth, rate, shape, dest, muted, symmetry) =
        storage.query(|s| (s.depth, s.rate, s.shape, s.dest, s.muted, s.symmetry));

    glob_depth.set(depth);
    glob_rate.set(rate);
    glob_shape.set(shape);
    glob_dest.set(dest);
    glob_muted.set(muted);
    glob_symmetry.set(symmetry);

    let in_jack = app.make_in_jack(0, in_range).await;
    let out_jacks = [
        app.make_out_jack(1, ranges[0]).await,
        app.make_out_jack(2, ranges[1]).await,
        app.make_out_jack(3, ranges[2]).await,
    ];

    paint_buttons(&leds, ch0_color(initial_lfo_active), dest, false, muted);

    let fut1 = async {
        let mut root_pos = 0.0f32;
        let mut stage_pos = [0.0f32; 3];
        let mut root_chaos = MorphChaos::new();
        let mut stage_chaos = [MorphChaos::new(); 3];
        let mut mute_gain = [4095u16; 3];
        let mut out_levels = [0u16; 3];
        let mut frozen = false;
        let mut frozen_value = 0u16;
        // u16::MAX is out of `midi_gate`'s range and forces one send per
        // channel on startup.
        let mut last_midi = [u16::MAX; 4];
        // One channel offered per iteration and rotated: at 8 ms that is at
        // most 125 messages/s in total, each channel refreshing at ~31 Hz.
        let mut midi_slot: usize = 0;

        let mut prev_raw_input = in_jack.get_value();
        // Starts idle so an unpatched Ripppple comes up on the internal LFO.
        let mut in_idle_ms = IN_IDLE_MS;
        let mut last_tick = ticker();
        let mut ms_since_tick = 0u16;
        let mut tick_period_ms = 21u16;

        loop {
            app.delay_millis(AUDIO_MS as u64).await;

            let raw_input = in_jack.get_value();

            // Patch heuristic: the jack has no sense pin, so a live cable is
            // inferred from movement. Deadband covers the ADC noise floor.
            if raw_input.abs_diff(prev_raw_input) > IN_DEADBAND {
                in_idle_ms = 0;
            } else {
                in_idle_ms = in_idle_ms.saturating_add(AUDIO_MS).min(IN_IDLE_MS);
            }
            prev_raw_input = raw_input;

            let lfo_active = in_idle_ms >= IN_IDLE_MS;
            glob_lfo_active.set(lfo_active);

            let flash = glob_btn_flash.modify(|f| {
                let mut arr = *f;
                for ms in arr.iter_mut() {
                    *ms = ms.saturating_sub(AUDIO_MS);
                }
                arr
            });

            let (morph, skew, lfo_clocked) = storage.query(|s| (s.morph, s.skew, s.lfo_clocked));
            let symmetry = glob_symmetry.get();

            let tick = ticker();
            if tick != last_tick {
                // Plausible tick gaps only: ignore the counter reset to u64::MAX
                // and anything slower than 2 s, which would be a stopped clock.
                if (AUDIO_MS..500).contains(&ms_since_tick) && tick > last_tick {
                    tick_period_ms = ms_since_tick;
                }
                last_tick = tick;
                ms_since_tick = 0;
            } else {
                ms_since_tick = ms_since_tick.saturating_add(AUDIO_MS);
            }

            // Keep the random-walk drift at its ~1 kHz rate even though the
            // loop only runs every AUDIO_MS.
            for _ in 0..AUDIO_MS {
                root_chaos.tick_walks(&die);
                for c in stage_chaos.iter_mut() {
                    c.tick_walks(&die);
                }
            }

            let held = glob_frozen.get();

            let root_sample = if lfo_active {
                let next_pos = if lfo_clocked {
                    // Phase locked to the tick counter, interpolated inside the
                    // tick. A stopped transport stops the counter, which parks
                    // the wave by itself.
                    if tick == u64::MAX {
                        0.0
                    } else {
                        let ticks_per_cycle = (glob_div.get() as u64)
                            .saturating_mul(lfo_speed_mult as u64)
                            .max(1);
                        let frac = (ms_since_tick as f32 / tick_period_ms.max(1) as f32).min(1.0);
                        let pos_ticks = (tick % ticks_per_cycle) as f32 + frac;
                        (pos_ticks * 4096.0 / ticks_per_cycle as f32) % 4096.0
                    }
                } else {
                    let step =
                        glob_lfo_step.get() * AUDIO_MS as f32 / lfo_speed_mult as f32;
                    if held {
                        root_pos
                    } else {
                        (root_pos + step) % 4096.0
                    }
                };

                let sample = morph_sample(
                    next_pos as usize,
                    morph,
                    (skew, LFO_WARP, symmetry),
                    0,
                    &mut root_chaos,
                    &die,
                );

                if !held {
                    root_pos = next_pos;
                }
                sample
            } else {
                raw_input
            };

            // Freeze snapshots the value so stepped morph nodes hold too, not
            // just the phase.
            if held != frozen {
                frozen = held;
                if frozen {
                    frozen_value = root_sample;
                }
            }
            let root_val = if frozen { frozen_value } else { root_sample };

            let midi_due = midi_out.is_some();
            if midi_due {
                midi_slot = (midi_slot + 1) % 4;
            }

            if midi_due && midi_slot == 0 {
                let gate_val = midi_gate(root_val, nrpn);
                if gate_val != last_midi[0] {
                    midi[0].try_send_cc(midi_ccs[0], root_val);
                    last_midi[0] = gate_val;
                }
            }

            let ch0_led_color = ch0_color(lfo_active);
            paint_bipolar_level(&leds, 0, ch0_led_color, root_val);

            let depth = glob_depth.get();
            let rate = glob_rate.get();
            let shape = glob_shape.get();
            let dest = glob_dest.get();
            let muted = glob_muted.get();

            // The cascade: every stage sees only its direct predecessor.
            let mut modulator = root_val;
            for i in 0..3 {
                let m = (modulator as f32 / 2047.5) - 1.0;
                let d = depth[i] as f32 / 4095.0;
                let mod_amt = m * d;
                let target = Dest::from_u8(dest[i]);

                // Rate modulation is exponential: a linear step would make the
                // top of the range a hair's movement and the bottom inert.
                let mut step = lfo_step(rate[i]) * AUDIO_MS as f32;
                if target == Dest::Rate {
                    step = (step * libm::exp2f(mod_amt * RATE_MOD_OCTAVES)).min(MAX_PHASE_STEP);
                }

                let effective_morph = if target == Dest::Shape {
                    (shape[i] as i32 + (mod_amt * 2047.0) as i32).clamp(0, 4095) as u16
                } else {
                    shape[i]
                };

                let mut sample = morph_sample(
                    stage_pos[i] as usize,
                    effective_morph,
                    (STAGE_SKEW, LFO_WARP, symmetry),
                    i % 2,
                    &mut stage_chaos[i],
                    &die,
                );

                if target == Dest::Depth {
                    // Classic AM: full amplitude at depth zero, ducking towards
                    // silence as the predecessor swings negative.
                    let gain_f = 1.0 - d * (1.0 - (m + 1.0) * 0.5);
                    let gain = (gain_f.clamp(0.0, 1.0) * 4095.0) as u16;
                    sample = attenuate_bipolar(sample, gain);
                }

                stage_pos[i] = (stage_pos[i] + step) % 4096.0;

                // Ramped rather than switched, so a mute does not click.
                mute_gain[i] = clickless(mute_gain[i], if muted[i] { 0 } else { 4095 });
                let level = if bipolar[i] {
                    attenuate_bipolar(sample, mute_gain[i])
                } else {
                    ((sample as u32 * mute_gain[i] as u32) / 4095) as u16
                };

                out_jacks[i].set_value(level);
                out_levels[i] = level;
                modulator = level;

                if midi_due && midi_slot == i + 1 {
                    let gate_val = midi_gate(level, nrpn);
                    if gate_val != last_midi[i + 1] {
                        midi[i + 1].try_send_cc(midi_ccs[i + 1], level);
                        last_midi[i + 1] = gate_val;
                    }
                }

                let color = target.color();
                if muted[i] {
                    leds.set(i + 1, Led::Top, color, Brightness::Low);
                    leds.set(i + 1, Led::Bottom, color, Brightness::Low);
                } else {
                    paint_bipolar_level(&leds, i + 1, color, level);
                }
            }

            if flash[0] == 0 {
                leds.set(
                    0,
                    Led::Button,
                    ch0_led_color,
                    if held {
                        BUTTON_BRIGHTNESS
                    } else {
                        // The LFO always swings around mid-scale, regardless of
                        // the configured input range.
                        signal_brightness(root_val, lfo_active || in_range.is_bipolar())
                    },
                );
            }

            // Keep metering while Shift holds Alt: the pulse rate is what the
            // Alt-layer rate fader is editing, so freezing the button bright
            // would hide the feedback.
            for i in 0..3 {
                if muted[i] {
                    leds.unset(i + 1, Led::Button);
                } else if flash[i + 1] == 0 {
                    leds.set(
                        i + 1,
                        Led::Button,
                        Dest::from_u8(dest[i]).color(),
                        signal_brightness(out_levels[i], bipolar[i]),
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
                if fader_val.abs_diff(at_down) > FADER_MOVE_THRESH {
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

            let target_value = if chan == 0 {
                match latch_active_layer {
                    LatchLayer::Main => storage.query(|s| s.lfo_speed),
                    LatchLayer::Alt => storage.query(|s| s.morph),
                    LatchLayer::Third => storage.query(|s| s.skew),
                }
            } else {
                let i = chan - 1;
                match latch_active_layer {
                    LatchLayer::Main => storage.query(|s| s.depth[i]),
                    LatchLayer::Alt => storage.query(|s| s.rate[i]),
                    // Ch3 trades its own shape slot for the shared symmetry.
                    LatchLayer::Third => {
                        if i == 2 {
                            storage.query(|s| s.symmetry)
                        } else {
                            storage.query(|s| s.shape[i])
                        }
                    }
                }
            };

            if let Some(new_value) = latch[chan].update(fader_val, latch_active_layer, target_value)
            {
                if chan == 0 {
                    match latch_active_layer {
                        LatchLayer::Main => {
                            storage.modify_and_save(|s| s.lfo_speed = new_value);
                            time_calc();
                        }
                        LatchLayer::Alt => {
                            storage.modify_and_save(|s| s.morph = new_value);
                        }
                        LatchLayer::Third => {
                            storage.modify_and_save(|s| s.skew = new_value);
                        }
                    }
                } else {
                    let i = chan - 1;
                    match latch_active_layer {
                        LatchLayer::Main => {
                            storage.modify_and_save(|s| s.depth[i] = new_value);
                            glob_depth.modify(|d| {
                                let mut arr = *d;
                                arr[i] = new_value;
                                arr
                            });
                        }
                        LatchLayer::Alt => {
                            storage.modify_and_save(|s| s.rate[i] = new_value);
                            glob_rate.modify(|r| {
                                let mut arr = *r;
                                arr[i] = new_value;
                                arr
                            });
                        }
                        LatchLayer::Third => {
                            if i == 2 {
                                storage.modify_and_save(|s| s.symmetry = new_value);
                                glob_symmetry.set(new_value);
                            } else {
                                storage.modify_and_save(|s| s.shape[i] = new_value);
                                glob_shape.modify(|sh| {
                                    let mut arr = *sh;
                                    arr[i] = new_value;
                                    arr
                                });
                            }
                        }
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
            glob_long_press.modify(|l| {
                let mut arr = *l;
                arr[chan] = false;
                arr
            });
        }
    };

    let button_long = async {
        loop {
            let (chan, shift) = buttons.wait_for_any_long_press().await;

            glob_long_press.modify(|l| {
                let mut arr = *l;
                arr[chan] = true;
                arr
            });

            if !(1..=3).contains(&chan) {
                continue;
            }
            let i = chan - 1;

            if shift {
                // 1 blink = ±5V, 2 blinks = 0–10V. Flash before restart so the
                // paint loop's hold-off can show it; wait for release and the
                // flash duration before reconfiguring the jack.
                let next = params.query(|p| match i {
                    0 => next_range(p.range_b),
                    1 => next_range(p.range_c),
                    _ => next_range(p.range_d),
                });
                let times = range_flash_times(next);
                let hold_ms = range_flash_hold_ms(times);
                leds.set_mode(
                    chan,
                    Led::Button,
                    LedMode::Flash(Dest::from_u8(glob_dest.get()[i]).color(), Some(times)),
                );
                glob_btn_flash.modify(|f| {
                    let mut arr = *f;
                    arr[chan] = hold_ms;
                    arr
                });
                join(
                    buttons.wait_for_up(chan),
                    app.delay_millis(hold_ms as u64),
                )
                .await;
                params
                    .update(|p| match i {
                        0 => p.range_b = next,
                        1 => p.range_c = next,
                        _ => p.range_d = next,
                    })
                    .await;
                restart.signal(());
            } else {
                let next = storage.modify_and_save(|s| {
                    s.dest[i] = Dest::from_u8(s.dest[i]).next().as_u8();
                    s.dest[i]
                });
                glob_dest.modify(|d| {
                    let mut arr = *d;
                    arr[i] = next;
                    arr
                });
                leds.set_mode(
                    chan,
                    Led::Button,
                    LedMode::Flash(Dest::from_u8(next).color(), Some(4)),
                );
                glob_btn_flash.modify(|f| {
                    let mut arr = *f;
                    arr[chan] = BUTTON_FLASH_MS;
                    arr
                });
                // Mirror into the param so configurator / Presetpunk follow the
                // device. `ParamStore::update` only saves and pushes; it does not
                // restart run(), so the LFO phases keep running through a target
                // change.
                params.update(|p| p.target[i] = next as usize).await;
            }
        }
    };

    let button_up = async {
        loop {
            let (chan, shift) = buttons.wait_for_any_up().await;

            if glob_fader_moved.get()[chan] || glob_long_press.get()[chan] {
                continue;
            }

            match chan {
                0 if shift && glob_lfo_active.get() => {
                    let clocked = storage.modify_and_save(|s| {
                        s.lfo_clocked = !s.lfo_clocked;
                        s.lfo_clocked
                    });
                    if clocked {
                        leds.set_mode(
                            0,
                            Led::Button,
                            LedMode::Flash(ch0_color(glob_lfo_active.get()), Some(4)),
                        );
                        glob_btn_flash.modify(|f| {
                            let mut arr = *f;
                            arr[0] = BUTTON_FLASH_MS;
                            arr
                        });
                    }
                }
                0 if !shift => {
                    let frozen = glob_frozen.toggle();
                    paint_buttons(
                        &leds,
                        ch0_color(glob_lfo_active.get()),
                        glob_dest.get(),
                        frozen,
                        glob_muted.get(),
                    );
                }
                // Shift stays reserved for the range swap: a shift-held tap must
                // never fall through to mute.
                1..=3 if !shift => {
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
                    paint_buttons(
                        &leds,
                        ch0_color(glob_lfo_active.get()),
                        glob_dest.get(),
                        glob_frozen.get(),
                        muted_all,
                    );
                }
                _ => {}
            }
        }
    };

    let fut3 = join3(button_down, button_up, button_long);

    let scene_handler = async {
        loop {
            match app.wait_for_scene_event().await {
                SceneEvent::LoadScene(scene) => {
                    storage.load_from_scene(scene).await;

                    let (depth, rate, shape, dest, muted, symmetry) = storage
                        .query(|s| (s.depth, s.rate, s.shape, s.dest, s.muted, s.symmetry));

                    glob_depth.set(depth);
                    glob_rate.set(rate);
                    glob_shape.set(shape);
                    glob_dest.set(dest);
                    glob_muted.set(muted);
                    glob_symmetry.set(symmetry);

                    time_calc();

                    paint_buttons(
                        &leds,
                        ch0_color(glob_lfo_active.get()),
                        dest,
                        glob_frozen.get(),
                        muted,
                    );
                }
                SceneEvent::SaveScene(scene) => storage.save_to_scene(scene).await,
            }
        }
    };

    select(join4(fut1, fut2, fut3, scene_handler), restart.wait()).await;
}
