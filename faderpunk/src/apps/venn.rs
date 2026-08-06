//! Venn — dual Euclidean layers combined with boolean logic (OR/AND/XOR/Accnt).
//!
//! Gate-only, no pitch. Inspired by OXI ONE MKII GEN page 2 (eLen2/ePul2/eRot2 + Logic).
//! Distinct from Euclid (single layer + aux) and GenSeq (Turing + pitch CV).

use embassy_futures::{
    join::join5,
    select::{select, select3, Either},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use heapless::Vec;
use libfp::{
    ext::FromValue, latch::LatchLayer, utils::euclidean_at, AppIcon, Brightness, ClockDivision,
    Color, Config, MidiChannel, MidiNote, MidiOut, Param, Value, APP_MAX_PARAMS,
};
use serde::{Deserialize, Serialize};

use crate::app::{
    App, AppParams, AppStorage, ClockEvent, Led, ManagedStorage, ParamStore, SceneEvent,
};
use crate::tasks::leds::LedMode;

pub const CHANNELS: usize = 2;
pub const PARAMS: usize = 9;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
/// Button hit soft-dip duration (1 ms ticks). Short so dense hits still blink
/// (long hold-offs stay stuck at 62% and look like no pulse).
const BUTTON_HIT_FLASH_MS: u16 = 40;
/// Hit remain light: dim 38% → keep 62% (no black flash).
const BUTTON_HIT_REMAIN_PCT: u8 = 62;
/// Hold off Main button paint so LedMode::Flash (logic cycle) can finish.
const BUTTON_FLASH_MS: u16 = 550;
/// eInv gesture LED feedback (white↔off), same as Heat Pump invert.
const EINVERT_FADE_MS: u16 = 500;
/// Floor / span for pulse-density brightness on buttons (Main).
const BTN_BRIGHT_FLOOR: u8 = 90;
const BTN_BRIGHT_SPAN: u8 = 165;
/// Floor for Alt rotation / Third length Top+Button meters (rot=0 still visible).
const LAYER_LED_FLOOR: u8 = 40;
/// Ignore tiny fader noise when deciding Btn2 tap-mute vs Third scrub.
const FADER_MOVE_THRESH: u16 = 64;
const RESOLUTION: [u32; 12] = [384, 192, 96, 48, 24, 16, 12, 8, 6, 4, 3, 2];

/// Boolean combine modes for the two Euclidean layers.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Logic {
    Or = 0,
    And = 1,
    Xor = 2,
    Accnt = 3,
}

impl Logic {
    fn from_u8(v: u8) -> Self {
        match v % 4 {
            0 => Self::Or,
            1 => Self::And,
            2 => Self::Xor,
            _ => Self::Accnt,
        }
    }

    fn next(self) -> Self {
        Self::from_u8(self as u8 + 1)
    }

    fn color(self) -> Color {
        match self {
            Self::Or => Color::Green,
            Self::And => Color::Yellow,
            Self::Xor => Color::Red,
            Self::Accnt => Color::White,
        }
    }
}

/// Auto-complement for Layer B LEDs from the Config Color (Layer A).
fn complement_color(c: Color) -> Color {
    match c {
        Color::Blue | Color::SkyBlue | Color::LightBlue => Color::Orange,
        Color::Green | Color::Lime | Color::PaleGreen => Color::Violet,
        Color::Rose | Color::Salmon => Color::Yellow,
        Color::Orange | Color::Sand => Color::Blue,
        Color::Cyan => Color::Pink,
        Color::Pink => Color::Cyan,
        Color::Violet => Color::Green,
        Color::Yellow => Color::Rose,
        Color::White | Color::Custom(_, _, _) => Color::Pink,
        Color::Red => Color::Cyan,
    }
}

fn length_from_fader(v: u16) -> u8 {
    // 2..=32 (Bjorklund table supports 2–32 steps)
    ((v as u32 * 31 / 4095) as u8).saturating_add(2).min(32)
}

fn pulses_from_fader(v: u16, len: u8) -> u8 {
    let len = len.max(1);
    ((v as u32 * len as u32 / 4095) as u8).min(len)
}

fn rotation_from_fader(v: u16, len: u8) -> u8 {
    let len = len.max(1);
    ((v as u32 * (len.saturating_sub(1)) as u32 / 4095) as u8) % len
}

fn length_band_color(len: u8) -> Color {
    if len <= 8 {
        Color::Red
    } else if len <= 16 {
        Color::Yellow
    } else {
        Color::Blue
    }
}

/// Button base brightness from fill density (pulses/length) — denser = brighter.
fn button_density_bright(pulses: u8, len: u8) -> u8 {
    let len = len.max(1) as u16;
    let fill = (pulses as u16 * 255) / len;
    BTN_BRIGHT_FLOOR + ((fill * BTN_BRIGHT_SPAN as u16) / 255) as u8
}

fn hit_dip_bright(base: u8) -> u8 {
    ((base as u16 * BUTTON_HIT_REMAIN_PCT as u16) / 100).min(255) as u8
}

/// Rotation meter: 0..=len-1 → floor..=255.
fn rot_meter_bright(rot: u8, len: u8) -> u8 {
    let len = len.max(1) as u16;
    let span = (255u16 - LAYER_LED_FLOOR as u16).max(1);
    let t = ((rot as u16 * span) / len).min(span);
    (LAYER_LED_FLOOR as u16 + t) as u8
}

/// Length meter: 2..=32 → floor..=255.
fn len_meter_bright(len: u8) -> u8 {
    let len = len.clamp(2, 32) as u16;
    let span = (255u16 - LAYER_LED_FLOOR as u16).max(1);
    let t = (((len - 2) * span) / 30).min(span);
    (LAYER_LED_FLOOR as u16 + t) as u8
}

pub static CONFIG: Config<PARAMS> = Config::new(
    "Venn",
    "Dual Euclidean layers with boolean logic (OR/AND/XOR/Accnt)",
    Color::Cyan,
    AppIcon::Euclid,
)
.add_param(Param::MidiChannel {
    name: "MIDI Channel",
})
.add_param(Param::MidiNote {
    name: "MIDI Note A",
})
.add_param(Param::MidiNote {
    name: "MIDI Note B",
})
.add_param(Param::MidiOut)
.add_param(Param::i32 {
    name: "Division",
    min: 1,
    max: 12,
})
.add_param(Param::i32 {
    name: "GATE %",
    min: 1,
    max: 100,
})
.add_param(Param::i32 {
    name: "Prob %",
    min: 0,
    max: 100,
})
.add_param(Param::i32 {
    name: "Vel %",
    min: 0,
    max: 100,
})
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
});

pub struct Params {
    midi_channel: MidiChannel,
    note_a: MidiNote,
    note_b: MidiNote,
    midi_out: MidiOut,
    division: i32,
    gatel: i32,
    prob: i32,
    vel: i32,
    color: Color,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            midi_channel: MidiChannel::default(),
            note_a: MidiNote::from(32),
            note_b: MidiNote::from(33),
            midi_out: MidiOut::default(),
            division: 5, // RESOLUTION[4] = 24 → 16ths at 24 PPQN
            gatel: 50,
            prob: 100,
            vel: 0,
            color: Color::Cyan,
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
            note_a: MidiNote::from_value(values[1]),
            note_b: MidiNote::from_value(values[2]),
            midi_out: MidiOut::from_value(values[3]),
            division: i32::from_value(values[4]),
            gatel: i32::from_value(values[5]),
            prob: i32::from_value(values[6]),
            vel: i32::from_value(values[7]),
            color: Color::from_value(values[8]),
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.midi_channel.into()).unwrap();
        vec.push(self.note_a.into()).unwrap();
        vec.push(self.note_b.into()).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.division.into()).unwrap();
        vec.push(self.gatel.into()).unwrap();
        vec.push(self.prob.into()).unwrap();
        vec.push(self.vel.into()).unwrap();
        vec.push(self.color.into()).unwrap();
        vec
    }
}

/// Fader layout:
///   F0 Main=pulses_a  Alt=rot_a  Third=len_a
///   F1 Main=pulses_b  Alt=rot_b  Third=len_b
///
/// Buttons:
///   Btn0/Btn1 hold = Third (lengths); Btn1 tap (no fader move) = mute
///   Shift+Btn1 = cycle Logic OR→AND→XOR→Accnt
///   Shift+Btn2 = toggle eInv (post-logic shadow); white↔none LED fade
///
/// Outputs:
///   Jack0 = logic result, Jack1 = coupled companion (see Logic table)
#[derive(Serialize, Deserialize)]
pub struct Storage {
    pulses_a: u16,
    pulses_b: u16,
    rot_a: u16,
    rot_b: u16,
    len_a: u16,
    len_b: u16,
    logic: u8,
    einv: bool,
    muted: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            // Mid-ish defaults: length 16, pulses ~7 / ~3, no rotation
            pulses_a: 1792,
            pulses_b: 768,
            rot_a: 0,
            rot_b: 0,
            len_a: 1840, // ~16
            len_b: 1840,
            logic: Logic::Or as u8,
            einv: false,
            muted: false,
        }
    }
}

impl AppStorage for Storage {}

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

fn div_from_param(division: i32) -> u32 {
    let idx = (division.clamp(1, 12) as usize) - 1;
    RESOLUTION[idx]
}

fn midi_velocity(base: u16, vel_pct: i32, die_roll: u16) -> u16 {
    if vel_pct <= 0 {
        return base;
    }
    let span = (base as u32 * vel_pct as u32 / 100) as u16;
    let half = span / 2;
    let offset = (die_roll as u32 * span as u32 / 4095) as i32 - half as i32;
    (base as i32 + offset).clamp(1, 4095) as u16
}

pub async fn run(
    app: &App<CHANNELS>,
    params: &ParamStore<Params>,
    storage: &ManagedStorage<Storage>,
) {
    app.wait_while_perf_muted().await;

    let mut clock = app.use_clock();
    let ticks = clock.get_ticker();
    let die = app.use_die();
    let faders = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();

    let (midi_out, midi_chan, note_a, note_b, division, gatel, prob, vel, color_a) =
        params.query(|p| {
            (
                p.midi_out,
                p.midi_channel,
                p.note_a,
                p.note_b,
                p.division,
                p.gatel,
                p.prob,
                p.vel,
                p.color,
            )
        });
    let color_b = complement_color(color_a);

    let midi = app.use_midi_output(midi_out, midi_chan, false);

    let jack = [
        app.make_gate_jack(0, 4095).await,
        app.make_gate_jack(1, 4095).await,
    ];

    let glob_muted = app.make_global(false);
    let glob_einv = app.make_global(false);
    let glob_logic = app.make_global(Logic::Or as u8);
    let glob_latch_layer = app.make_global(LatchLayer::Main);

    let len_a_glob = app.make_global(16u8);
    let len_b_glob = app.make_global(16u8);
    let pulses_a_glob = app.make_global(7u8);
    let pulses_b_glob = app.make_global(3u8);
    let rot_a_glob = app.make_global(0u8);
    let rot_b_glob = app.make_global(0u8);
    // Main button pulse flashes (1 ms countdown while Flash plays).
    let glob_btn_a_flash = app.make_global(0u16);
    let glob_btn_b_flash = app.make_global(0u16);
    let glob_btn_flash = app.make_global(0u16);
    // Remaining ms of eInv LED fade; 0 = inactive.
    let glob_einv_fade = app.make_global(0u16);
    // true = none→white, false = white→none.
    let glob_einv_fade_up = app.make_global(false);
    let glob_fader_moved = app.make_global(false);

    let div = div_from_param(division);

    // Load storage → globals
    {
        let s = storage.query(|s| {
            (
                s.pulses_a,
                s.pulses_b,
                s.rot_a,
                s.rot_b,
                s.len_a,
                s.len_b,
                s.logic,
                s.einv,
                s.muted,
            )
        });
        let (pa, pb, ra, rb, la, lb, logic, einv, muted) = s;
        let len_a = length_from_fader(la);
        let len_b = length_from_fader(lb);
        len_a_glob.set(len_a);
        len_b_glob.set(len_b);
        pulses_a_glob.set(pulses_from_fader(pa, len_a));
        pulses_b_glob.set(pulses_from_fader(pb, len_b));
        rot_a_glob.set(rotation_from_fader(ra, len_a));
        rot_b_glob.set(rotation_from_fader(rb, len_b));
        glob_logic.set(Logic::from_u8(logic) as u8);
        glob_einv.set(einv);
        glob_muted.set(muted);
    }

    // Initial button LEDs
    leds.set(
        0,
        Led::Button,
        Logic::from_u8(glob_logic.get()).color(),
        LED_BRIGHTNESS,
    );
    if glob_muted.get() {
        leds.unset(1, Led::Button);
    } else {
        leds.set(1, Led::Button, color_b, LED_BRIGHTNESS);
    }

    let fut_pulse = async {
        let mut note_on_a = false;
        let mut note_on_b = false;

        loop {
            match clock.wait_for_event(ClockDivision::_1).await {
                ClockEvent::Reset | ClockEvent::Stop => {
                    midi.send_note_off(note_a).await;
                    midi.send_note_off(note_b).await;
                    note_on_a = false;
                    note_on_b = false;
                    jack[0].set_low().await;
                    jack[1].set_low().await;
                    leds.unset(0, Led::Top);
                    leds.unset(1, Led::Top);
                    leds.unset(0, Led::Bottom);
                    leds.unset(1, Led::Bottom);
                }
                ClockEvent::Tick => {
                    let clkn = ticks() as u32;
                    let muted = glob_muted.get();
                    let latch = glob_latch_layer.get();
                    let logic = Logic::from_u8(glob_logic.get());
                    let einv = glob_einv.get();

                    let len_a = len_a_glob.get().max(2);
                    let len_b = len_b_glob.get().max(2);
                    let pulses_a = pulses_a_glob.get().min(len_a);
                    let pulses_b = pulses_b_glob.get().min(len_b);
                    let rot_a = rot_a_glob.get() % len_a;
                    let rot_b = rot_b_glob.get() % len_b;

                    if clkn.is_multiple_of(div) {
                        let step = clkn / div;
                        // euclidean_at(num_steps=length, num_beats=pulses, …)
                        let a = euclidean_at(len_a, pulses_a, rot_a, step);
                        let b = euclidean_at(len_b, pulses_b, rot_b, step);

                        let (mut out0, mut out1) = match logic {
                            Logic::Or => (a || b, a && b),
                            Logic::And => (a && b, a ^ b),
                            Logic::Xor => (a ^ b, a && b),
                            Logic::Accnt => (a, a && b),
                        };
                        if einv {
                            out0 = !out0;
                            out1 = !out1;
                        }

                        // Optional Prob % (100 = deterministic)
                        if prob < 100 {
                            let thr = (prob.clamp(0, 100) as u32 * 4095 / 100) as u16;
                            if out0 && die.roll() > thr {
                                out0 = false;
                            }
                            if out1 && die.roll() > thr {
                                out1 = false;
                            }
                        }

                        if !muted {
                            if out0 {
                                let vel_a = midi_velocity(4095, vel, die.roll());
                                midi.send_note_on(note_a, vel_a).await;
                                jack[0].set_high().await;
                                note_on_a = true;
                            }
                            if out1 {
                                let vel_b = midi_velocity(4095, vel, die.roll());
                                midi.send_note_on(note_b, vel_b).await;
                                jack[1].set_high().await;
                                note_on_b = true;
                            }
                        }

                        // Layer hits: soft dim on buttons (62% remain); Top/Bottom keep Flash.
                        if a {
                            glob_btn_a_flash.set(BUTTON_HIT_FLASH_MS);
                        }
                        if b && !muted {
                            glob_btn_b_flash.set(BUTTON_HIT_FLASH_MS);
                        }
                        if latch == LatchLayer::Main {
                            if a {
                                leds.set_mode(0, Led::Top, LedMode::Flash(color_a, Some(1)));
                            }
                            if b {
                                leds.set_mode(1, Led::Top, LedMode::Flash(color_b, Some(1)));
                            }
                            if out0 && !muted {
                                leds.set_mode(
                                    0,
                                    Led::Bottom,
                                    LedMode::Flash(logic.color(), Some(1)),
                                );
                            }
                            if out1 && !muted {
                                leds.set_mode(
                                    1,
                                    Led::Bottom,
                                    LedMode::Flash(logic.color(), Some(1)),
                                );
                            }
                        }

                        // Alt: step-0 sync markers on Bottom
                        if latch == LatchLayer::Alt {
                            let on_a0 = step.is_multiple_of(len_a as u32);
                            let on_b0 = step.is_multiple_of(len_b as u32);
                            if on_a0 {
                                leds.set(0, Led::Bottom, color_a, Brightness::High);
                            } else {
                                leds.unset(0, Led::Bottom);
                            }
                            if on_b0 {
                                leds.set(1, Led::Bottom, color_b, Brightness::High);
                            } else {
                                leds.unset(1, Led::Bottom);
                            }
                        }
                    }

                    // Gate off
                    let gate_off = (div * gatel as u32 / 100).clamp(1, div.saturating_sub(1));
                    if clkn % div == gate_off {
                        if note_on_a {
                            midi.send_note_off(note_a).await;
                            note_on_a = false;
                            jack[0].set_low().await;
                        }
                        if note_on_b {
                            midi.send_note_off(note_b).await;
                            note_on_b = false;
                            jack[1].set_low().await;
                        }
                    }
                    // Alt/Third fader meters paint in the 1 ms loop (not clock-bound).
                }
                _ => {}
            }
        }
    };

    let fut_buttons = async {
        loop {
            let (chan, shift) = buttons.wait_for_any_down().await;
            if shift {
                if chan == 0 {
                    // Cycle Logic
                    let next = Logic::from_u8(glob_logic.get()).next();
                    glob_logic.set(next as u8);
                    storage.modify_and_save(|s| {
                        s.logic = next as u8;
                    });
                    leds.set_mode(0, Led::Button, LedMode::Flash(next.color(), Some(2)));
                    glob_btn_flash.set(BUTTON_FLASH_MS);
                } else if chan == 1 {
                    // Toggle eInv — white↔none fade like Heat Pump invert.
                    let einv = !glob_einv.get();
                    glob_einv.set(einv);
                    storage.modify_and_save(|s| {
                        s.einv = einv;
                    });
                    // Invert on → white→none; invert off → none→white.
                    glob_btn_b_flash.set(0);
                    glob_einv_fade_up.set(!einv);
                    glob_einv_fade.set(EINVERT_FADE_MS);
                }
            } else if chan == 1 {
                // Btn2: hold+fader = Third (length); tap (no fader move) = mute.
                // Mute must not fire on down — that made hold look like a dead/black button.
                glob_fader_moved.set(false);
                let start0 = faders.get_value_at(0);
                let start1 = faders.get_value_at(1);
                loop {
                    match select(buttons.wait_for_up(1), faders.wait_for_any_change()).await {
                        Either::First(_) => break,
                        Either::Second(_) => {
                            if faders.get_value_at(0).abs_diff(start0) > FADER_MOVE_THRESH
                                || faders.get_value_at(1).abs_diff(start1) > FADER_MOVE_THRESH
                            {
                                glob_fader_moved.set(true);
                            }
                        }
                    }
                }
                if !glob_fader_moved.get() {
                    let muted = glob_muted.toggle();
                    storage.modify_and_save(|s| {
                        s.muted = muted;
                    });
                    if muted {
                        jack[0].set_low().await;
                        jack[1].set_low().await;
                        midi.send_note_off(note_a).await;
                        midi.send_note_off(note_b).await;
                        leds.unset(1, Led::Button);
                    } else {
                        leds.set(1, Led::Button, color_b, LED_BRIGHTNESS);
                    }
                }
            }
            // Btn0/Btn1 hold = Third layer via shift task (length A/B)
        }
    };

    let fut_latch = async {
        let mut latch = [
            app.make_latch(faders.get_value_at(0)),
            app.make_latch(faders.get_value_at(1)),
        ];
        loop {
            let chan = faders.wait_for_any_change().await;
            let layer = glob_latch_layer.get();
            let target = match (chan, layer) {
                (0, LatchLayer::Main) => storage.query(|s| s.pulses_a),
                (0, LatchLayer::Alt) => storage.query(|s| s.rot_a),
                (0, LatchLayer::Third) => storage.query(|s| s.len_a),
                (1, LatchLayer::Main) => storage.query(|s| s.pulses_b),
                (1, LatchLayer::Alt) => storage.query(|s| s.rot_b),
                (1, LatchLayer::Third) => storage.query(|s| s.len_b),
                _ => 0,
            };
            if let Some(new_value) =
                latch[chan].update(faders.get_value_at(chan), layer, target)
            {
                match (chan, layer) {
                    (0, LatchLayer::Main) => {
                        let len = len_a_glob.get();
                        pulses_a_glob.set(pulses_from_fader(new_value, len));
                        storage.modify_and_save(|s| s.pulses_a = new_value);
                    }
                    (0, LatchLayer::Alt) => {
                        let len = len_a_glob.get();
                        rot_a_glob.set(rotation_from_fader(new_value, len));
                        storage.modify_and_save(|s| s.rot_a = new_value);
                    }
                    (0, LatchLayer::Third) => {
                        let len = length_from_fader(new_value);
                        len_a_glob.set(len);
                        // Re-clamp pulses/rot against new length
                        let pa = storage.query(|s| s.pulses_a);
                        let ra = storage.query(|s| s.rot_a);
                        pulses_a_glob.set(pulses_from_fader(pa, len));
                        rot_a_glob.set(rotation_from_fader(ra, len));
                        storage.modify_and_save(|s| s.len_a = new_value);
                    }
                    (1, LatchLayer::Main) => {
                        let len = len_b_glob.get();
                        pulses_b_glob.set(pulses_from_fader(new_value, len));
                        storage.modify_and_save(|s| s.pulses_b = new_value);
                    }
                    (1, LatchLayer::Alt) => {
                        let len = len_b_glob.get();
                        rot_b_glob.set(rotation_from_fader(new_value, len));
                        storage.modify_and_save(|s| s.rot_b = new_value);
                    }
                    (1, LatchLayer::Third) => {
                        let len = length_from_fader(new_value);
                        len_b_glob.set(len);
                        let pb = storage.query(|s| s.pulses_b);
                        let rb = storage.query(|s| s.rot_b);
                        pulses_b_glob.set(pulses_from_fader(pb, len));
                        rot_b_glob.set(rotation_from_fader(rb, len));
                        storage.modify_and_save(|s| s.len_b = new_value);
                    }
                    _ => {}
                }
            }
        }
    };

    let fut_scene = async {
        loop {
            match app.wait_for_scene_event().await {
                SceneEvent::LoadScene(scene) => {
                    storage.load_from_scene(scene).await;
                    let (pa, pb, ra, rb, la, lb, logic, einv, muted) = storage.query(|s| {
                        (
                            s.pulses_a,
                            s.pulses_b,
                            s.rot_a,
                            s.rot_b,
                            s.len_a,
                            s.len_b,
                            s.logic,
                            s.einv,
                            s.muted,
                        )
                    });
                    let len_a = length_from_fader(la);
                    let len_b = length_from_fader(lb);
                    len_a_glob.set(len_a);
                    len_b_glob.set(len_b);
                    pulses_a_glob.set(pulses_from_fader(pa, len_a));
                    pulses_b_glob.set(pulses_from_fader(pb, len_b));
                    rot_a_glob.set(rotation_from_fader(ra, len_a));
                    rot_b_glob.set(rotation_from_fader(rb, len_b));
                    glob_logic.set(Logic::from_u8(logic) as u8);
                    glob_einv.set(einv);
                    glob_muted.set(muted);

                    leds.set(
                        0,
                        Led::Button,
                        Logic::from_u8(logic).color(),
                        LED_BRIGHTNESS,
                    );
                    if muted {
                        leds.unset(1, Led::Button);
                    } else {
                        leds.set(1, Led::Button, color_b, LED_BRIGHTNESS);
                    }
                }
                SceneEvent::SaveScene(scene) => {
                    storage.save_to_scene(scene).await;
                }
            }
        }
    };

    let fut_shift = async {
        let mut prev_latch = LatchLayer::Main;
        loop {
            app.delay_millis(1).await;

            // Third = hold Btn1 or Btn2 (length); Alt = Shift without channel button.
            let latch_active = if buttons.is_shift_pressed()
                && !buttons.is_button_pressed(0)
                && !buttons.is_button_pressed(1)
            {
                LatchLayer::Alt
            } else if !buttons.is_shift_pressed()
                && (buttons.is_button_pressed(0) || buttons.is_button_pressed(1))
            {
                LatchLayer::Third
            } else {
                LatchLayer::Main
            };
            glob_latch_layer.set(latch_active);

            // Leaving meters: Main clears all; Alt clears Bottom (Third bands).
            if prev_latch != latch_active {
                match latch_active {
                    LatchLayer::Main => {
                        leds.unset(0, Led::Top);
                        leds.unset(1, Led::Top);
                        leds.unset(0, Led::Bottom);
                        leds.unset(1, Led::Bottom);
                    }
                    LatchLayer::Alt => {
                        leds.unset(0, Led::Bottom);
                        leds.unset(1, Led::Bottom);
                    }
                    LatchLayer::Third => {}
                }
            }
            prev_latch = latch_active;

            // Button LED paint: density base; soft hit dip; skip while logic Flash plays.
            let flash_left = glob_btn_flash.get();
            if flash_left > 0 {
                glob_btn_flash.set(flash_left.saturating_sub(1));
            }
            // Decrement after sampling so this frame still paints the dip.
            let hit_a = glob_btn_a_flash.get();
            let hit_b = glob_btn_b_flash.get();
            if hit_a > 0 {
                glob_btn_a_flash.set(hit_a.saturating_sub(1));
            }
            if hit_b > 0 {
                glob_btn_b_flash.set(hit_b.saturating_sub(1));
            }

            // eInv feedback (white↔off) on Btn2 — mirrors Heat Pump invert.
            let einv_fade = glob_einv_fade.get();
            if einv_fade > 0 {
                let elapsed = EINVERT_FADE_MS.saturating_sub(einv_fade);
                let bright = if glob_einv_fade_up.get() {
                    ((elapsed as u32 * 255) / EINVERT_FADE_MS as u32) as u8
                } else {
                    (((EINVERT_FADE_MS - elapsed) as u32 * 255) / EINVERT_FADE_MS as u32) as u8
                };
                leds.set(1, Led::Button, Color::White, Brightness::Custom(bright));
                let next = einv_fade.saturating_sub(1);
                glob_einv_fade.set(next);
                if next == 0 {
                    if glob_muted.get() {
                        leds.unset(1, Led::Button);
                    } else {
                        let len_b = len_b_glob.get().max(1);
                        let base_b = button_density_bright(pulses_b_glob.get(), len_b);
                        leds.set(1, Led::Button, color_b, Brightness::Custom(base_b));
                    }
                }
            }

            let len_a = len_a_glob.get().max(1);
            let len_b = len_b_glob.get().max(1);
            let muted = glob_muted.get();

            match latch_active {
                // Hit dips always paint on Main (even during logic Flash hold-off on the other path).
                LatchLayer::Main => {
                    if flash_left > 0 {
                        // Logic Flash owns Btn1; still allow Btn2 hit dips.
                        if muted {
                            if einv_fade == 0 {
                                leds.unset(1, Led::Button);
                            }
                        } else if einv_fade == 0 {
                            let base_b = button_density_bright(pulses_b_glob.get(), len_b);
                            let bright_b = if hit_b > 0 {
                                hit_dip_bright(base_b)
                            } else {
                                base_b
                            };
                            leds.set(1, Led::Button, color_b, Brightness::Custom(bright_b));
                        }
                    } else {
                        let base_a = button_density_bright(pulses_a_glob.get(), len_a);
                        let base_b = button_density_bright(pulses_b_glob.get(), len_b);
                        let bright_a = if hit_a > 0 {
                            hit_dip_bright(base_a)
                        } else {
                            base_a
                        };
                        leds.set(
                            0,
                            Led::Button,
                            Logic::from_u8(glob_logic.get()).color(),
                            Brightness::Custom(bright_a),
                        );
                        if muted {
                            if einv_fade == 0 {
                                leds.unset(1, Led::Button);
                            }
                        } else if einv_fade == 0 {
                            let bright_b = if hit_b > 0 {
                                hit_dip_bright(base_b)
                            } else {
                                base_b
                            };
                            leds.set(1, Led::Button, color_b, Brightness::Custom(bright_b));
                        }
                    }
                }
                LatchLayer::Alt if flash_left == 0 => {
                    // Rotation meters: Top + buttons, layer hues, floor so rot=0 stays lit.
                    let ba = rot_meter_bright(rot_a_glob.get(), len_a);
                    let bb = rot_meter_bright(rot_b_glob.get(), len_b);
                    leds.set(0, Led::Top, color_a, Brightness::Custom(ba));
                    leds.set(1, Led::Top, color_b, Brightness::Custom(bb));
                    leds.set(
                        0,
                        Led::Button,
                        Logic::from_u8(glob_logic.get()).color(),
                        Brightness::Custom(ba),
                    );
                    if muted {
                        if einv_fade == 0 {
                            leds.unset(1, Led::Button);
                        }
                    } else if einv_fade == 0 {
                        leds.set(1, Led::Button, color_b, Brightness::Custom(bb));
                    }
                }
                LatchLayer::Third if flash_left == 0 => {
                    // Length meters: white Top ∝ len, Bottom band, buttons track band+len.
                    let ba = len_meter_bright(len_a);
                    let bb = len_meter_bright(len_b);
                    leds.set(0, Led::Top, Color::White, Brightness::Custom(ba));
                    leds.set(1, Led::Top, Color::White, Brightness::Custom(bb));
                    leds.set(0, Led::Bottom, length_band_color(len_a), Brightness::Mid);
                    leds.set(1, Led::Bottom, length_band_color(len_b), Brightness::Mid);
                    leds.set(
                        0,
                        Led::Button,
                        length_band_color(len_a),
                        Brightness::Custom(ba),
                    );
                    if muted {
                        if einv_fade == 0 {
                            leds.unset(1, Led::Button);
                        }
                    } else if einv_fade == 0 {
                        leds.set(
                            1,
                            Led::Button,
                            length_band_color(len_b),
                            Brightness::Custom(bb),
                        );
                    }
                }
                _ => {}
            }
        }
    };

    join5(fut_pulse, fut_buttons, fut_latch, fut_scene, fut_shift).await;
}
