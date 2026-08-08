//! Contura — melodic contour over selectable 12-TET pitch-class sets.
//!
//! Generates phrases with mixed note lengths. Scale sets are conventional
//! interval-pattern labels (not claims about living musical practice).
//! Optional follow of the device quantizer tonic / scale.

use embassy_futures::{
    join::{join, join5},
    select::{select, select3},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use heapless::Vec;
use midly::num::u7;
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue,
    latch::LatchLayer,
    quantizer::Pitch,
    AppIcon, Brightness, ClockDivision, Color, Config, Key, MidiChannel, MidiNote, MidiOut, Note,
    Param, Range, Value, VoltPerOct, APP_MAX_PARAMS,
};

use crate::{
    app::{
        App, AppParams, AppStorage, ClockEvent, Die, Led, ManagedStorage, ParamStore, SceneEvent,
    },
    tasks::global_config::get_global_config,
};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 10;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
const OCTAVE_BLINK_MS: u16 = 250;
const BUTTON_DUCK_MS: u16 = 25;

const MIN_PHRASE: u8 = 4;
const MAX_PHRASE: u8 = 24;
const POOL_CAP: usize = 48;

/// Clock divisions (24 PPQN ticks). Index matches Division param.
/// Clock divisions (24 PPQN ticks). Index matches Division param.
const RESOLUTION: [u32; 12] = [384, 192, 96, 48, 24, 16, 12, 8, 6, 4, 3, 2];
const DIV_LABELS: &[&str] = &[
    "1/1", "1/2", "1/4", "1/8", "1/16", "1/24", "1/32", "1/48", "1/64", "1/96", "1/128", "1/192",
];

const OCT_COLORS: [Color; 4] = [Color::Blue, Color::Cyan, Color::Yellow, Color::Red];

/// Button / cue hues cycle by scale index — not by geography.
const SET_COLORS: [Color; 8] = [
    Color::Orange,
    Color::Cyan,
    Color::Violet,
    Color::Rose,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Pink,
];

// Pitch-class masks, MSB = C … LSB = B (same layout as `Key::as_u16_key`).
const MASK_INSEN: u16 = 0b110010010010;
const MASK_YO: u16 = 0b101010010100;
const MASK_HIRAJOSHI: u16 = 0b110010100010;
const MASK_BHAIRAV: u16 = 0b110101011001;
const MASK_KAFI: u16 = 0b101101010110;
const MASK_BHUPALI: u16 = 0b101010010100;
const MASK_HIJAZ: u16 = 0b110011011010;
const MASK_BAYATI: u16 = 0b110101011010;
const MASK_RAST: u16 = 0b101011010110;
const MASK_GAMELAN: u16 = 0b110100011000;
const MASK_HUNGARIAN: u16 = 0b101100111001;
const MASK_FOLK: u16 = 0b110111011010;

/// Flat list of named 12-TET sets (Western modes and other named collections,
/// same footing). Labels are conventional interval-pattern names only.
const SCALE_LABELS: &[&str] = &[
    "Ionian",
    "Dorian",
    "Phrygian",
    "Mixolydian",
    "Aeolian",
    "Pent Maj",
    "Pent Min",
    "Blues Min",
    "In Sen",
    "Yo",
    "Hirajoshi",
    "Bhairav",
    "Kafi",
    "Bhupali",
    "Hijaz",
    "Bayati",
    "Rast",
    "Gamelan",
    "Hungarian",
    "Folk",
];

const SCALE_MASKS: [u16; 20] = [
    0b101011010101, // Ionian
    0b101101010110, // Dorian
    0b110101011010, // Phrygian
    0b101011010110, // Mixolydian
    0b101101011010, // Aeolian
    0b101010010100, // Pent Maj
    0b100101010010, // Pent Min
    0b100101110010, // Blues Min
    MASK_INSEN,
    MASK_YO,
    MASK_HIRAJOSHI,
    MASK_BHAIRAV,
    MASK_KAFI,
    MASK_BHUPALI,
    MASK_HIJAZ,
    MASK_BAYATI,
    MASK_RAST,
    MASK_GAMELAN,
    MASK_HUNGARIAN,
    MASK_FOLK,
];

const SCALE_COUNT: usize = 20;

pub static CONFIG: Config<PARAMS> = Config::new(
    "Contura",
    "Melodic contour over selectable 12-TET scale sets, anchored to the device tonic",
    Color::Orange,
    AppIcon::Note,
)
.add_param(Param::MidiChannel {
    name: "MIDI Channel",
})
.add_param(Param::MidiNote { name: "Base Note" })
.add_param(Param::Color {
    name: "Color",
    variants: &[
        Color::Orange,
        Color::Violet,
        Color::Cyan,
        Color::Rose,
        Color::Blue,
        Color::Green,
        Color::Pink,
        Color::Yellow,
    ],
})
.add_param(Param::MidiOut)
.add_param(Param::Range {
    name: "Range",
    variants: &[Range::_0_10V, Range::_0_5V, Range::_Neg5_5V],
})
.add_param(Param::VoltPerOct)
.add_param(Param::bool {
    name: "Follow device tonic",
})
.add_param(Param::bool {
    name: "Follow device scale",
})
.add_param(Param::Enum {
    name: "Scale set",
    variants: &SCALE_LABELS,
})
.add_param(Param::Enum {
    name: "Division",
    variants: &DIV_LABELS,
});

pub struct Params {
    midi_channel: MidiChannel,
    base_note: MidiNote,
    color: Color,
    midi_out: MidiOut,
    range: Range,
    vpo: VoltPerOct,
    follow_tonic: bool,
    follow_scale: bool,
    scale_set: usize,
    division: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            midi_channel: MidiChannel::default(),
            base_note: MidiNote::from(48),
            color: Color::Orange,
            midi_out: MidiOut::default(),
            range: Range::_0_10V,
            vpo: VoltPerOct::Standard,
            follow_tonic: true,
            follow_scale: false,
            scale_set: 0, // Ionian
            division: 4,  // 1/16
        }
    }
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < PARAMS {
            return None;
        }
        Some(Self {
            midi_channel: MidiChannel::from_value(values[0]),
            base_note: MidiNote::from_value(values[1]),
            color: Color::from_value(values[2]),
            midi_out: MidiOut::from_value(values[3]),
            range: Range::from_value(values[4]),
            vpo: VoltPerOct::from_value(values[5]),
            follow_tonic: bool::from_value(values[6]),
            follow_scale: bool::from_value(values[7]),
            scale_set: usize::from_value(values[8]).min(SCALE_COUNT - 1),
            division: usize::from_value(values[9]).min(RESOLUTION.len() - 1),
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.midi_channel.into()).unwrap();
        vec.push(self.base_note.into()).unwrap();
        vec.push(self.color.into()).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.vpo.into()).unwrap();
        vec.push(self.follow_tonic.into()).unwrap();
        vec.push(self.follow_scale.into()).unwrap();
        vec.push(self.scale_set.into()).unwrap();
        vec.push(self.division.into()).unwrap();
        vec
    }
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Storage {
    /// Main fader: max interval width (scale degrees).
    interval_saved: u16,
    /// Shift fader: phrase length + density macro.
    phrase_saved: u16,
    /// Button+fader: expressivity (repeat ↔ leap / long notes).
    express_saved: u16,
    scale_set: u8,
    octaves: u8,
    muted: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            interval_saved: 2048,
            phrase_saved: 2048,
            express_saved: 2048,
            scale_set: 0,
            octaves: 2,
            muted: false,
        }
    }
}

impl AppStorage for Storage {}

fn clamp_octaves(o: u8) -> u8 {
    o.clamp(1, 4)
}

fn cycle_octaves(o: u8) -> u8 {
    let o = clamp_octaves(o);
    if o >= 4 {
        1
    } else {
        o + 1
    }
}

fn set_color(idx: u8) -> Color {
    SET_COLORS[idx as usize % SET_COLORS.len()]
}

fn midi_u8(note: MidiNote) -> u8 {
    u7::from(note).as_int()
}

fn note_to_pitch(note: u8) -> Pitch {
    let octave = (note as i16 / 12) - 1;
    let pc = note % 12;
    Pitch {
        octave: octave as i8,
        note: Note::from(pc),
        raw: None,
    }
}

fn degrees_from_mask(mask: u16) -> Vec<u8, 12> {
    let mut out = Vec::new();
    for i in 0..12u8 {
        if (mask >> (11 - i)) & 1 != 0 {
            let _ = out.push(i);
        }
    }
    if out.is_empty() {
        let _ = out.push(0);
    }
    out
}

fn active_mask(follow_scale: bool, scale_set: usize) -> u16 {
    if follow_scale {
        let key = get_global_config().quantizer.key;
        if key == Key::Off {
            Key::Chromatic.as_u16_key()
        } else {
            key.as_u16_key()
        }
    } else {
        SCALE_MASKS[scale_set.min(SCALE_COUNT - 1)]
    }
}

fn active_tonic(follow_tonic: bool, base: MidiNote) -> u8 {
    if follow_tonic {
        get_global_config().quantizer.tonic as u8
    } else {
        midi_u8(base) % 12
    }
}

fn build_pool(mask: u16, tonic: u8, base: u8, octaves: u8) -> Vec<u8, POOL_CAP> {
    let degrees = degrees_from_mask(mask);
    let lo = base;
    let hi = (base as u16 + octaves as u16 * 12).min(127) as u8;
    let mut pool = Vec::new();
    for oct in -2i16..=8 {
        for &deg in degrees.iter() {
            let semi = oct * 12 + deg as i16 + tonic as i16;
            if !(0..=127).contains(&semi) {
                continue;
            }
            let n = semi as u8;
            if n >= lo && n <= hi {
                let _ = pool.push(n);
            }
        }
    }
    if pool.is_empty() {
        let _ = pool.push(base.clamp(0, 127));
    }
    pool
}

fn phrase_from_fader(v: u16) -> u8 {
    let span = (MAX_PHRASE - MIN_PHRASE) as u32;
    (MIN_PHRASE as u32 + (v as u32 * span) / 4095) as u8
}

fn density_from_fader(v: u16) -> u16 {
    900 + ((v as u32 * 3195) / 4095) as u16
}

fn max_step_from_fader(v: u16, pool_len: usize) -> usize {
    let max = (pool_len / 2).clamp(1, 8);
    1 + ((v as usize * (max.saturating_sub(1))) / 4095)
}

fn pick_duration(die: &Die, express: u16, remain: u8) -> u8 {
    let roll = die.roll();
    let long_bias = express;
    let dur = if roll < 1200 + (4095 - long_bias) / 2 {
        1
    } else if roll < 2800 {
        2 + (die.roll() % 3) as u8
    } else {
        (remain / 2).max(3).min(remain.max(1))
    };
    dur.clamp(1, remain.max(1))
}

fn pick_next_index(
    die: &Die,
    cur: usize,
    pool_len: usize,
    max_step: usize,
    express: u16,
    rising: bool,
) -> usize {
    if pool_len <= 1 {
        return 0;
    }
    let repeat_chance = 4095u16.saturating_sub(express) / 3;
    if die.roll() < repeat_chance {
        return cur;
    }
    let step = 1 + (die.roll() as usize % max_step.max(1));
    let signed = if rising { step as i16 } else { -(step as i16) };
    let signed = if express > 2800 && die.roll() < 600 {
        -signed
    } else {
        signed
    };
    let next = cur as i16 + signed;
    next.clamp(0, pool_len as i16 - 1) as usize
}

#[embassy_executor::task(pool_size = 16 / CHANNELS)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(app.app_id, app.layout_id, Params::default());
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
    let (
        midi_out,
        midi_chan,
        base_note,
        led_color,
        range,
        vpo,
        follow_tonic,
        follow_scale,
        scale_param,
        division,
    ) = params.query(|p| {
        (
            p.midi_out,
            p.midi_channel,
            p.base_note,
            p.color,
            p.range,
            p.vpo,
            p.follow_tonic,
            p.follow_scale,
            p.scale_set,
            p.division,
        )
    });

    let buttons = app.use_buttons();
    let faders = app.use_faders();
    let leds = app.use_leds();
    let mut clock = app.use_clock();
    let ticks = clock.get_ticker();
    let die = app.use_die();
    let midi = app.use_midi_output(midi_out, midi_chan, false);
    let cv = app.make_out_jack(0, range).await;

    let (interval0, phrase0, express0, scale0, octaves0, muted0) = storage.query(|s| {
        (
            s.interval_saved,
            s.phrase_saved,
            s.express_saved,
            s.scale_set,
            s.octaves,
            s.muted,
        )
    });

    let scale_init = if scale0 == 0 {
        scale_param as u8
    } else {
        scale0
    }
    .min((SCALE_COUNT - 1) as u8);

    let glob_interval = app.make_global(interval0);
    let glob_phrase = app.make_global(phrase0);
    let glob_express = app.make_global(express0);
    let glob_div = app.make_global(RESOLUTION[division.min(RESOLUTION.len() - 1)]);
    let glob_scale = app.make_global(scale_init);
    let glob_octaves = app.make_global(clamp_octaves(octaves0));
    let glob_muted = app.make_global(muted0);
    let glob_latch = app.make_global(LatchLayer::Main);
    let glob_fader_moved = app.make_global(false);
    let glob_octave_blink = app.make_global(0u16);
    let glob_button_duck = app.make_global(0u16);
    let long_press_fired = app.make_global(false);

    if muted0 {
        leds.unset(0, Led::Button);
    } else {
        leds.set(0, Led::Button, set_color(scale_init), LED_BRIGHTNESS);
    }

    // Clock → flags only; voice owns MIDI/CV (never await MIDI in clock path).
    let pending_fire = app.make_global(false);
    let pending_note = app.make_global(0u8);
    let pending_note_off = app.make_global(false);
    let pending_silence = app.make_global(false);
    let glob_gate_on = app.make_global(false);

    let fut_clock = async {
        let mut pool: Vec<u8, POOL_CAP> = Vec::new();
        let mut phrase_step: u8 = 0;
        let mut remain: u8 = 0;
        let mut rising = true;
        let mut gated = false;
        let rebuild = |pool: &mut Vec<u8, POOL_CAP>, scale_set: u8, octaves: u8| -> usize {
            let mask = active_mask(follow_scale, scale_set as usize);
            let tonic = active_tonic(follow_tonic, base_note);
            let base = midi_u8(base_note);
            *pool = build_pool(mask, tonic, base, octaves);
            pool.len()
        };

        let mut last_scale = glob_scale.get();
        let mut last_oct = glob_octaves.get();
        let plen0 = rebuild(&mut pool, last_scale, last_oct);
        let mut idx = plen0 / 3;

        loop {
            match clock.wait_for_event(ClockDivision::_1).await {
                ClockEvent::Reset | ClockEvent::Stop => {
                    remain = 0;
                    phrase_step = 0;
                    gated = false;
                    pending_fire.set(false);
                    pending_note_off.set(false);
                    pending_silence.set(true);
                    glob_gate_on.set(false);
                    leds.unset(0, Led::Top);
                    leds.unset(0, Led::Bottom);
                }
                ClockEvent::Start => {}
                ClockEvent::Tick => {
                    let div = glob_div.get().max(1);
                    let tick = ticks();
                    if !tick.is_multiple_of(div as u64) {
                        continue;
                    }

                    let muted = glob_muted.get();
                    let scale_set = glob_scale.get();
                    let octaves = glob_octaves.get();
                    let interval = glob_interval.get();
                    let phrase_f = glob_phrase.get();
                    let express = glob_express.get();

                    if scale_set != last_scale || octaves != last_oct {
                        let plen = rebuild(&mut pool, scale_set, octaves);
                        last_scale = scale_set;
                        last_oct = octaves;
                        idx = idx.min(plen.saturating_sub(1));
                    }
                    let plen = pool.len().max(1);
                    let phrase_len = phrase_from_fader(phrase_f).max(1);
                    let density = density_from_fader(phrase_f);
                    let max_step = max_step_from_fader(interval, plen);
                    idx = idx.min(plen.saturating_sub(1));

                    if muted {
                        if gated {
                            pending_note_off.set(true);
                            gated = false;
                        }
                        remain = 0;
                        glob_gate_on.set(false);
                        continue;
                    }

                    if phrase_step == 0 {
                        rising = true;
                    } else if phrase_step >= phrase_len / 2 {
                        rising = false;
                    }

                    // Hold path: count down first so a new note's duration is not
                    // consumed on the same step it starts.
                    if remain > 0 {
                        remain -= 1;
                        if remain == 0 && gated {
                            pending_note_off.set(true);
                            gated = false;
                            glob_gate_on.set(false);
                        }
                    } else if die.roll() > density {
                        // Rest for one division step.
                        if gated {
                            pending_note_off.set(true);
                            gated = false;
                            glob_gate_on.set(false);
                        }
                        remain = 1;
                    } else {
                        let steps_left = phrase_len.saturating_sub(phrase_step).max(1);
                        if phrase_step == 0 || steps_left <= 2 {
                            if die.roll() < 2800 {
                                idx = idx.saturating_sub(max_step.min(idx));
                            }
                        } else {
                            idx = pick_next_index(&die, idx, plen, max_step, express, rising);
                        }

                        remain = pick_duration(&die, express, steps_left).max(1);
                        let note = pool[idx.min(plen - 1)];
                        pending_note.set(note);
                        pending_fire.set(true);
                        gated = true;
                        glob_gate_on.set(true);
                        glob_button_duck.set(BUTTON_DUCK_MS);
                    }

                    phrase_step = phrase_step.wrapping_add(1);
                    if phrase_step >= phrase_len {
                        phrase_step = 0;
                    }
                }
            }
        }
    };

    let fut_voice = async {
        let mut note_on: Option<u8> = None;
        loop {
            app.delay_millis(1).await;

            if pending_silence.get() {
                pending_silence.set(false);
                pending_fire.set(false);
                pending_note_off.set(false);
                if let Some(n) = note_on.take() {
                    midi.send_note_off(MidiNote::from(n)).await;
                }
                cv.set_value(0);
                leds.unset(0, Led::Top);
                leds.unset(0, Led::Bottom);
                continue;
            }

            if pending_note_off.get() {
                pending_note_off.set(false);
                if let Some(n) = note_on.take() {
                    midi.send_note_off(MidiNote::from(n)).await;
                }
                // Keep pitch CV; clear gate cue.
                leds.unset(0, Led::Top);
            }

            if pending_fire.get() {
                pending_fire.set(false);
                if glob_muted.get() {
                    continue;
                }
                let note = pending_note.get();
                if let Some(old) = note_on {
                    if old != note {
                        midi.send_note_off(MidiNote::from(old)).await;
                    }
                }
                let pitch = note_to_pitch(note);
                cv.set_value(pitch.as_counts(range, vpo));
                midi.send_note_on(MidiNote::from(note), 3200).await;
                note_on = Some(note);
                leds.set(0, Led::Top, led_color, Brightness::High);
                let bright = Brightness::Custom(((note as u16).saturating_mul(2)).min(255) as u8);
                leds.set(0, Led::Bottom, led_color, bright);
            }
        }
    };

    let fut_faders = async {
        let mut latch = app.make_latch(faders.get_value());
        loop {
            faders.wait_for_change_at(0).await;
            let layer = glob_latch.get();
            if layer == LatchLayer::Third {
                glob_fader_moved.set(true);
            }

            let target = match layer {
                LatchLayer::Main => storage.query(|s| s.interval_saved),
                LatchLayer::Alt => storage.query(|s| s.phrase_saved),
                LatchLayer::Third => storage.query(|s| s.express_saved),
            };

            if let Some(v) = latch.update(faders.get_value(), layer, target) {
                match layer {
                    LatchLayer::Main => {
                        glob_interval.set(v);
                        storage.modify_and_save(|s| s.interval_saved = v);
                    }
                    LatchLayer::Alt => {
                        glob_phrase.set(v);
                        storage.modify_and_save(|s| s.phrase_saved = v);
                    }
                    LatchLayer::Third => {
                        glob_express.set(v);
                        storage.modify_and_save(|s| s.express_saved = v);
                    }
                }
            }
        }
    };

    let fut_buttons = async {
        loop {
            buttons.wait_for_any_down().await;
            let shift = buttons.is_shift_pressed();
            long_press_fired.set(false);
            glob_fader_moved.set(false);
            buttons.wait_for_up(0).await;

            if long_press_fired.get() {
                continue;
            }

            if shift {
                // Shift+short: previous scale set.
                let scale = glob_scale.get() as usize;
                let prev = if scale == 0 {
                    SCALE_COUNT - 1
                } else {
                    scale - 1
                };
                glob_scale.set(prev as u8);
                storage.modify_and_save(|s| s.scale_set = prev as u8);
                leds.set(0, Led::Bottom, set_color(prev as u8), Brightness::High);
            } else if !glob_fader_moved.get() {
                let muted = glob_muted.toggle();
                storage.modify_and_save(|s| s.muted = muted);
                if muted {
                    leds.unset(0, Led::Button);
                    leds.unset(0, Led::Top);
                    leds.unset(0, Led::Bottom);
                } else {
                    leds.set(
                        0,
                        Led::Button,
                        set_color(glob_scale.get()),
                        LED_BRIGHTNESS,
                    );
                }
            }
        }
    };

    let fut_long = async {
        loop {
            buttons.wait_for_any_long_press().await;
            long_press_fired.set(true);

            if buttons.is_shift_pressed() {
                let oct = cycle_octaves(glob_octaves.get());
                glob_octaves.set(oct);
                storage.modify_and_save(|s| s.octaves = oct);
                leds.set(0, Led::Top, OCT_COLORS[(oct - 1) as usize], Brightness::High);
                glob_octave_blink.set(OCTAVE_BLINK_MS);
            } else if !glob_fader_moved.get() {
                // Long: next scale set.
                let next = (glob_scale.get() as usize + 1) % SCALE_COUNT;
                glob_scale.set(next as u8);
                storage.modify_and_save(|s| s.scale_set = next as u8);
                leds.set(0, Led::Button, set_color(next as u8), Brightness::High);
            }
        }
    };

    let fut_leds = async {
        loop {
            app.delay_millis(1).await;

            let layer = if buttons.is_shift_pressed() && !buttons.is_button_pressed(0) {
                LatchLayer::Alt
            } else if !buttons.is_shift_pressed() && buttons.is_button_pressed(0) {
                LatchLayer::Third
            } else {
                LatchLayer::Main
            };
            glob_latch.set(layer);

            if glob_octave_blink.get() > 0 {
                glob_octave_blink.set(glob_octave_blink.get().saturating_sub(1));
                if glob_octave_blink.get() == 0 {
                    leds.unset(0, Led::Top);
                }
            }
            if glob_button_duck.get() > 0 {
                glob_button_duck.set(glob_button_duck.get().saturating_sub(1));
                if !glob_muted.get() {
                    let bright = if glob_button_duck.get() > 0 {
                        Brightness::Low
                    } else {
                        LED_BRIGHTNESS
                    };
                    leds.set(0, Led::Button, set_color(glob_scale.get()), bright);
                }
            }

            // Leave Top/Bottom to the voice engine on Main so note cues stay visible.
            match layer {
                LatchLayer::Alt => {
                    leds.set(0, Led::Bottom, Color::White, Brightness::Low);
                }
                LatchLayer::Third => {
                    leds.set(0, Led::Bottom, set_color(glob_scale.get()), Brightness::Low);
                }
                LatchLayer::Main => {
                    if glob_gate_on.get() {
                        leds.set(0, Led::Top, led_color, Brightness::Mid);
                    }
                }
            }
        }
    };

    let fut_scene = async {
        loop {
            match app.wait_for_scene_event().await {
                SceneEvent::LoadScene(_) => {
                    let (i, p, e, s, o, m) = storage.query(|st| {
                        (
                            st.interval_saved,
                            st.phrase_saved,
                            st.express_saved,
                            st.scale_set,
                            st.octaves,
                            st.muted,
                        )
                    });
                    glob_interval.set(i);
                    glob_phrase.set(p);
                    glob_express.set(e);
                    glob_scale.set(s.min((SCALE_COUNT - 1) as u8));
                    glob_octaves.set(clamp_octaves(o));
                    glob_muted.set(m);
                    let div = params.query(|p| p.division);
                    glob_div.set(RESOLUTION[div.min(RESOLUTION.len() - 1)]);
                }
                SceneEvent::SaveScene(_) => {}
            }
        }
    };

    join5(
        join(fut_clock, fut_voice),
        fut_faders,
        join(fut_buttons, fut_long),
        fut_leds,
        fut_scene,
    )
    .await;
}
