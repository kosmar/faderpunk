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
    utils::{attenuate_bipolar, split_unsigned_value, value_to_index},
    AppIcon, Brightness, ClockDivision, Color, Config, MidiChannel, MidiNote, MidiOut, Param,
    Range, Value, APP_MAX_PARAMS,
};

use crate::app::{
    App, AppParams, AppStorage, ClockEvent, Led, ManagedStorage, ParamStore, SceneEvent,
};
use crate::apps::genre_palette::{genre_fader_center, GENRE_NAMES, NUM_GENRES};
use crate::apps::groove::{
    feel_curve, feel_lerp_i32, feel_lerp_u16, swing_bias, swing_delay_ticks, FLAT_VEL, SIXTEENTH,
};
use crate::apps::led_fx::{genre_nearest, genre_pair, lerp_i32, lerp_u8, spectrum_color};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 15;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
/// Reverse-swing LED feedback (white↔off), same as Heat Pump / Golden Gate.
const REVERSE_FADE_MS: u16 = 500;
/// Ignore tiny ADC noise when deciding "button+fader scrub" vs long-press mute.
const FADER_MOVE_THRESH: u16 = 64;

/// 16 sixteenths per 4/4 bar.
const STEPS_PER_BAR: u32 = 16;

const JACK_ANY: u8 = 0;
const JACK_STACKED: u8 = 1;

const CV_JACK_OUT: usize = 0;
const CV_JACK_IN: usize = 1;

const DEST_DENSITY: usize = 0;
const DEST_FEEL: usize = 1;
const DEST_RESET: usize = 2;
const DEST_COUNT: usize = 3;

const TRIG_HIGH: u16 = 2458;

/// Bitmasks: bit N = 16th step N in a bar (0 = downbeat).
struct Pattern {
    kick: u16,
    snare: u16,
    /// Always-on hats for this genre.
    hats: u16,
    /// Extra kick hits revealed progressively as density rises.
    kick_fill: u16,
    /// Extra snare/ghost hits revealed progressively as density rises.
    snare_fill: u16,
    /// Extra hats revealed progressively as density rises.
    hats_fill: u16,
    kick_base: u8,
    kick_accent: u8,
    kick_acc_mask: u16,
    snare_base: u8,
    snare_accent: u8,
    snare_acc_mask: u16,
    hats_base: u8,
    hats_accent: u8,
    hats_acc_mask: u16,
    /// Per-16th microtiming in PPQN ticks (−2..=+4 typical at Feel=max).
    timing: [i8; 16],
}

/// Oldest → newest. Indices match Shift+Fader buckets and Enum param.
const PATTERNS: [Pattern; NUM_GENRES] = [
    // 0 Dub — sparse kick, snare 2&4, thin offbeat hats; heavy 1 accent, laid-back
    Pattern {
        kick: 0b0000_0001_0000_0001,
        snare: 0b0001_0000_0001_0000,
        hats: 0b0100_0100_0100_0100,
        kick_fill: 0b0000_0100_0000_0000,
        snare_fill: 0b0000_0000_1000_0000,
        hats_fill: 0b0010_0010_0010_0010,
        kick_base: 55,
        kick_accent: 100,
        kick_acc_mask: 0b0000_0000_0000_0001,
        snare_base: 50,
        snare_accent: 95,
        snare_acc_mask: 0b0001_0000_0001_0000,
        hats_base: 40,
        hats_accent: 70,
        hats_acc_mask: 0b0100_0000_0100_0000,
        timing: [0, 1, 0, 2, 1, 2, 0, 2, 0, 1, 0, 2, 1, 2, 0, 2],
    },
    // 1 Disco — 4-on-floor, clap 2&4, offbeat hats
    Pattern {
        kick: 0b0001_0001_0001_0001,
        snare: 0b0001_0000_0001_0000,
        hats: 0b0100_0100_0100_0100,
        kick_fill: 0b0000_0100_0000_0100,
        snare_fill: 0b0100_0000_0100_0000,
        hats_fill: 0b1010_1010_1010_1010,
        kick_base: 75,
        kick_accent: 100,
        kick_acc_mask: 0b0001_0001_0001_0001,
        snare_base: 55,
        snare_accent: 100,
        snare_acc_mask: 0b0001_0000_0001_0000,
        hats_base: 45,
        hats_accent: 85,
        hats_acc_mask: 0b0100_0100_0100_0100,
        timing: [0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2],
    },
    // 2 Hip-Hop — boom-bap; strong 1+3 kick accents
    Pattern {
        kick: 0b0100_0001_0010_0001,
        snare: 0b0001_0000_0001_0000,
        hats: 0b0101_0101_0101_0101,
        kick_fill: 0b0100_0000_0100_0000,
        snare_fill: 0b0000_0100_0000_0100,
        hats_fill: 0b1111_1111_1111_1111,
        kick_base: 50,
        kick_accent: 100,
        kick_acc_mask: 0b0000_0001_0000_0001,
        snare_base: 55,
        snare_accent: 98,
        snare_acc_mask: 0b0001_0000_0001_0000,
        hats_base: 40,
        hats_accent: 75,
        hats_acc_mask: 0b0001_0001_0001_0001,
        timing: [0, 2, 1, 3, 0, 2, 1, 3, 0, 2, 1, 3, 1, 3, 2, 4],
    },
    // 3 House — classic 4-on-floor, clap 2&4
    Pattern {
        kick: 0b0001_0001_0001_0001,
        snare: 0b0001_0000_0001_0000,
        hats: 0b0100_0100_0100_0100,
        kick_fill: 0b0000_0100_0000_0100,
        snare_fill: 0b0100_0000_0100_0000,
        hats_fill: 0b1111_1111_1111_1111,
        kick_base: 80,
        kick_accent: 100,
        kick_acc_mask: 0b0001_0001_0001_0001,
        snare_base: 55,
        snare_accent: 100,
        snare_acc_mask: 0b0001_0000_0001_0000,
        hats_base: 45,
        hats_accent: 80,
        hats_acc_mask: 0b0100_0100_0100_0100,
        timing: [0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2],
    },
    // 4 Techno — straight kick; Feel via hat dynamics
    Pattern {
        kick: 0b0001_0001_0001_0001,
        snare: 0b0001_0000_0000_0000,
        hats: 0b0101_0101_0101_0101,
        kick_fill: 0b0100_0100_0100_0100,
        snare_fill: 0b0000_0001_0000_0000,
        hats_fill: 0b1111_1111_1111_1111,
        kick_base: 85,
        kick_accent: 100,
        kick_acc_mask: 0b0001_0001_0001_0001,
        snare_base: 50,
        snare_accent: 95,
        snare_acc_mask: 0b0001_0000_0000_0000,
        hats_base: 40,
        hats_accent: 90,
        hats_acc_mask: 0b0001_0001_0001_0001,
        timing: [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1],
    },
    // 5 Trip-Hop — laid-back, late snare pocket
    Pattern {
        kick: 0b0000_0001_0000_0001,
        snare: 0b0000_0000_0001_0000,
        hats: 0b0100_0000_0100_0000,
        kick_fill: 0b0000_0000_0001_0000,
        snare_fill: 0b0000_1000_0000_0000,
        hats_fill: 0b0100_0100_0100_0100,
        kick_base: 50,
        kick_accent: 95,
        kick_acc_mask: 0b0000_0000_0000_0001,
        snare_base: 45,
        snare_accent: 90,
        snare_acc_mask: 0b0000_0000_0001_0000,
        hats_base: 35,
        hats_accent: 65,
        hats_acc_mask: 0b0100_0000_0100_0000,
        timing: [0, 2, 1, 3, 2, 3, 1, 3, 0, 2, 1, 3, 3, 4, 2, 3],
    },
    // 6 UK Garage — skippy kick/hats
    Pattern {
        kick: 0b1000_1001_0010_0001,
        snare: 0b0001_0000_0001_0000,
        hats: 0b0110_0100_0110_0100,
        kick_fill: 0b0010_0000_0100_0000,
        snare_fill: 0b0000_0010_0000_0100,
        hats_fill: 0b1110_1101_1110_1101,
        kick_base: 50,
        kick_accent: 100,
        kick_acc_mask: 0b0000_0001_0000_0001,
        snare_base: 55,
        snare_accent: 98,
        snare_acc_mask: 0b0001_0000_0001_0000,
        hats_base: 40,
        hats_accent: 85,
        hats_acc_mask: 0b0100_0100_0100_0100,
        timing: [0, 3, -1, 2, 0, 3, 1, 4, 0, 3, -1, 2, 0, 3, 1, 4],
    },
    // 7 Dubstep — half-time: kick 1, snare 3
    Pattern {
        kick: 0b0000_0000_0000_0001,
        snare: 0b0000_0001_0000_0000,
        hats: 0b0100_0100_0000_0100,
        kick_fill: 0b0000_0000_0100_0000,
        snare_fill: 0b0000_1000_0000_0000,
        hats_fill: 0b0101_0100_0101_0100,
        kick_base: 60,
        kick_accent: 100,
        kick_acc_mask: 0b0000_0000_0000_0001,
        snare_base: 55,
        snare_accent: 100,
        snare_acc_mask: 0b0000_0001_0000_0000,
        hats_base: 40,
        hats_accent: 75,
        hats_acc_mask: 0b0100_0000_0000_0100,
        timing: [0, 1, 0, 2, 1, 2, 0, 2, 0, 1, 0, 2, 1, 2, 0, 2],
    },
];

pub static CONFIG: Config<PARAMS> = Config::new(
    "Grooves",
    "Multi-genre MIDI drum grooves with feel",
    Color::Orange,
    AppIcon::Die,
)
.add_param(Param::MidiNote {
    name: "MIDI Note Kick",
})
.add_param(Param::MidiChannel {
    name: "MIDI Channel Kick",
})
.add_param(Param::MidiNote {
    name: "MIDI Note Snare",
})
.add_param(Param::MidiChannel {
    name: "MIDI Channel Snare",
})
.add_param(Param::MidiNote {
    name: "MIDI Note Hats",
})
.add_param(Param::MidiChannel {
    name: "MIDI Channel Hats",
})
.add_param(Param::Enum {
    name: "Groove",
    variants: GENRE_NAMES,
})
.add_param(Param::i32 {
    name: "Swing max %",
    min: 10,
    max: 100,
})
.add_param(Param::i32 {
    name: "GATE %",
    min: 1,
    max: 100,
})
.add_param(Param::Color {
    name: "Color",
    variants: &[
        Color::Orange,
        Color::Yellow,
        Color::Pink,
        Color::Cyan,
        Color::Violet,
        Color::Green,
        Color::Blue,
        Color::Rose,
    ],
})
.add_param(Param::MidiOut)
.add_param(Param::Enum {
    name: "Jack",
    variants: &["CV Out", "CV In"],
})
.add_param(Param::Range {
    name: "Range",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Enum {
    name: "CV Dest",
    variants: &["Density", "Feel", "Reset"],
})
.add_param(Param::i32 {
    name: "CV Att",
    min: 0,
    max: 100,
});

pub struct Params {
    note_kick: MidiNote,
    midi_channel_kick: MidiChannel,
    note_snare: MidiNote,
    midi_channel_snare: MidiChannel,
    note_hats: MidiNote,
    midi_channel_hats: MidiChannel,
    genre: usize,
    swing_max_pct: i32,
    gatel: i32,
    color: Color,
    midi_out: MidiOut,
    cv_jack: usize,
    range: Range,
    cv_dest: usize,
    cv_att: i32,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < 11 {
            return None;
        }
        let (cv_jack, range, cv_dest, cv_att) = if values.len() >= PARAMS {
            (
                usize::from_value(values[11]).min(1),
                Range::from_value(values[12]),
                usize::from_value(values[13]).min(DEST_COUNT - 1),
                i32::from_value(values[14]).clamp(0, 100),
            )
        } else {
            (CV_JACK_OUT, Range::_0_10V, DEST_DENSITY, 100)
        };
        Some(Self {
            note_kick: MidiNote::from_value(values[0]),
            midi_channel_kick: MidiChannel::from_value(values[1]),
            note_snare: MidiNote::from_value(values[2]),
            midi_channel_snare: MidiChannel::from_value(values[3]),
            note_hats: MidiNote::from_value(values[4]),
            midi_channel_hats: MidiChannel::from_value(values[5]),
            genre: usize::from_value(values[6]).min(NUM_GENRES - 1),
            swing_max_pct: i32::from_value(values[7]).clamp(10, 100),
            gatel: i32::from_value(values[8]),
            color: Color::from_value(values[9]),
            midi_out: MidiOut::from_value(values[10]),
            cv_jack,
            range,
            cv_dest,
            cv_att,
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.note_kick.into()).unwrap();
        vec.push(self.midi_channel_kick.into()).unwrap();
        vec.push(self.note_snare.into()).unwrap();
        vec.push(self.midi_channel_snare.into()).unwrap();
        vec.push(self.note_hats.into()).unwrap();
        vec.push(self.midi_channel_hats.into()).unwrap();
        vec.push(self.genre.into()).unwrap();
        vec.push(self.swing_max_pct.into()).unwrap();
        vec.push(self.gatel.into()).unwrap();
        vec.push(self.color.into()).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.cv_jack.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.cv_dest.into()).unwrap();
        vec.push(self.cv_att.into()).unwrap();
        vec
    }
}

fn att_from_pct(pct: i32) -> u16 {
    ((pct.clamp(0, 100) as u32 * 4095) / 100) as u16
}

fn mod_u16(base: u16, in_val: u16) -> u16 {
    (base as i32 + in_val as i32 - 2047).clamp(0, 4095) as u16
}

#[derive(Serialize, Deserialize)]
pub struct Storage {
    /// Groove feel attenuator (0 = flat/grid, 4095 = full genre character).
    /// Same FRAM slot as the former `swing` field.
    feel: u16,
    /// Groove density: progressively reveals extra kick/snare/hat hits
    /// across the whole pattern (not just hats) as this rises.
    density: u16,
    jack_mode: u8,
    reversed: bool,
    muted: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            // Mid-high so a fresh instance already grooves.
            feel: 2800,
            density: 2048,
            jack_mode: JACK_ANY,
            reversed: false,
            muted: false,
        }
    }
}

impl AppStorage for Storage {}

fn bit_set(mask: u16, step: u32) -> bool {
    mask & (1u16 << (step % STEPS_PER_BAR)) != 0
}

/// Continuous "groove density" reveal for one voice's fill mask. Returns
/// `Some(frac)` if `step`'s fill bit should sound at this density, where
/// `frac` is 0..=255: 255 = fully revealed, lower = still fading in as the
/// fader crosses this bit's reveal point. Bits are revealed in step order,
/// one at a time, so every notch of fader movement changes *something* —
/// no hard density-zone jumps.
fn fill_reveal(fill: u16, density: u16, step: u32) -> Option<u8> {
    let bit = 1u16 << (step % STEPS_PER_BAR);
    if fill & bit == 0 {
        return None;
    }
    let total = fill.count_ones();
    if total == 0 {
        return None;
    }
    let mut rank = 0u32;
    for i in 0..(step % STEPS_PER_BAR) {
        if fill & (1u16 << i) != 0 {
            rank += 1;
        }
    }
    // Fixed-point (x256) count of fill bits revealed so far at this density.
    let revealed_scaled = (density as u32) * total * 256 / 4095;
    let revealed_count = revealed_scaled / 256;
    let frac = (revealed_scaled % 256) as u8;
    if rank < revealed_count {
        Some(255)
    } else if rank == revealed_count && frac > 0 {
        Some(frac)
    } else {
        None
    }
}

/// Scales a velocity percent between `quiet` (a ghost note the instant it's
/// revealed) and `full` (fully faded in) as `frac` (0..=255) rises.
fn ghost_vel_pct(frac: u8, quiet: u16, full: u16) -> u16 {
    quiet + ((full - quiet) as u32 * frac as u32 / 255) as u16
}

/// Extra micro-timing push for ghost-only steps (no core hit landing on the
/// same step): as density rises, revealed ghosts drag a little further
/// behind the grid, like a drummer digging into the pocket. Capped at a
/// fraction of the swing tick budget so it can never overtake the next step.
fn ghost_drag_ticks(density: u16) -> u32 {
    (density as u32 * 2) / 4095
}

fn midi_vel(mult: u16) -> u16 {
    // mult is 0..=100 "percent" of full scale
    ((4095u32 * mult as u32) / 100).min(4095) as u16
}

/// Core-hit velocity % from Pattern DNA, attenuated by Feel.
fn core_vel_pct(base: u8, accent: u8, acc_mask: u16, step: u32, feel: u16) -> u16 {
    let character = if bit_set(acc_mask, step) {
        u16::from(accent)
    } else {
        u16::from(base)
    };
    feel_lerp_u16(FLAT_VEL, character, feel)
}

/// Effective swing % from bias × Feel, capped by Swing max %.
fn feel_swing_pct(bias: u8, feel: u16, swing_max_pct: i32) -> i32 {
    let f = u32::from(feel_curve(feel));
    let pct = (u32::from(bias) * f) / 4095;
    pct.min(swing_max_pct.clamp(10, 100) as u32) as i32
}

/// Resting CV for activity pulses. On ±5V, 0 counts = −5V — use mid (0V) as idle
/// so Echolot / other gates aren't stuck seeing a permanent low.
fn pulse_idle(range: Range) -> u16 {
    match range {
        Range::_Neg5_5V => 2047,
        _ => 0,
    }
}

/// Map a 0–10V-style activity level onto the configured jack range.
/// ±5V: park idle at 0V (2047) and put hits in the positive half only.
fn pulse_on_range(unipolar: u16, range: Range) -> u16 {
    match range {
        Range::_Neg5_5V => {
            if unipolar == 0 {
                2047
            } else {
                2047u16.saturating_add(unipolar / 2).min(4095)
            }
        }
        _ => unipolar,
    }
}

fn any_pulse_level(kick: bool, snare: bool, hats: bool, range: Range) -> u16 {
    let mut level = 0u16;
    if hats {
        level = level.max(1400);
    }
    if snare {
        level = level.max(2600);
    }
    if kick {
        level = level.max(4095);
    }
    pulse_on_range(level, range)
}

fn stacked_pulse_level(kick: bool, snare: bool, hats: bool, range: Range) -> u16 {
    // ~1V / 2V / 4V on 0–10V (4095 ≈ 10V); remapped for ±5V via pulse_on_range.
    let mut units = 0u16;
    if hats {
        units += 1;
    }
    if snare {
        units += 2;
    }
    if kick {
        units += 4;
    }
    let uni = ((units as u32 * 4095) / 10).min(4095) as u16;
    pulse_on_range(uni, range)
}

#[embassy_executor::task(pool_size = 4)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(
        app.app_id,
        app.layout_id,
        Params {
            note_kick: MidiNote::from(36),
            midi_channel_kick: MidiChannel::default(),
            note_snare: MidiNote::from(38),
            midi_channel_snare: MidiChannel::default(),
            note_hats: MidiNote::from(42),
            midi_channel_hats: MidiChannel::default(),
            genre: 3, // House
            swing_max_pct: 50,
            // Short drum triggers (see TRIG_MAX_TICKS); 100% → 2 PPQN ticks.
            gatel: 100,
            color: Color::Orange,
            midi_out: MidiOut::default(),
            cv_jack: CV_JACK_OUT,
            range: Range::_0_10V,
            cv_dest: DEST_DENSITY,
            cv_att: 100,
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
    app.wait_while_perf_muted().await;

    let (
        midi_out,
        note_kick,
        note_snare,
        note_hats,
        midi_channel_kick,
        midi_channel_snare,
        midi_channel_hats,
        genre,
        swing_max_pct,
        gatel,
        led_color,
        cv_jack,
        range,
        cv_dest,
        cv_att,
    ) = params.query(|p| {
        (
            p.midi_out,
            p.note_kick,
            p.note_snare,
            p.note_hats,
            p.midi_channel_kick,
            p.midi_channel_snare,
            p.midi_channel_hats,
            p.genre.min(NUM_GENRES - 1),
            p.swing_max_pct.clamp(10, 100),
            p.gatel,
            p.color,
            p.cv_jack.min(1),
            p.range,
            p.cv_dest.min(DEST_COUNT - 1),
            att_from_pct(p.cv_att),
        )
    });

    let mut clock = app.use_clock();
    let ticks = clock.get_ticker();
    let faders = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
    let midi_kick = app.use_midi_output(midi_out, midi_channel_kick, false);
    let midi_snare = app.use_midi_output(midi_out, midi_channel_snare, false);
    let midi_hats = app.use_midi_output(midi_out, midi_channel_hats, false);
    let out_jack = if cv_jack == CV_JACK_OUT {
        Some(app.make_out_jack(0, range).await)
    } else {
        None
    };
    let in_jack = if cv_jack == CV_JACK_IN {
        Some(app.make_in_jack(0, Range::_Neg5_5V).await)
    } else {
        None
    };
    if let Some(ref jack) = out_jack {
        jack.set_value(pulse_idle(range));
    }

    let (feel, density, jack_mode, reversed, muted) =
        storage.query(|s| (s.feel, s.density, s.jack_mode, s.reversed, s.muted));

    let glob_feel = app.make_global(feel);
    let glob_swing_max = app.make_global(swing_max_pct);
    let glob_density = app.make_global(density);
    let glob_jack_mode = app.make_global(jack_mode);
    let glob_reversed = app.make_global(reversed);
    let glob_genre = app.make_global(genre);
    // Continuous Alt-layer fader value (not reconstructed from genre index).
    let glob_genre_fader = app.make_global(genre_fader_center(genre, NUM_GENRES));
    let glob_muted = app.make_global(muted);
    let glob_reset = app.make_global(false);
    let glob_cv_val = app.make_global(2047u16);
    let long_press_fired = app.make_global(false);
    let glob_fader_moved = app.make_global(false);
    let glob_fader_at_down = app.make_global(0u16);
    // Genre changed on device; persist ParamStore off the fader hot-path.
    let glob_genre_dirty = app.make_global(false);
    let glob_latch_layer = app.make_global(LatchLayer::Main);
    let glob_reverse_fade = app.make_global(0u16);
    let glob_reverse_fade_up = app.make_global(false);
    let glob_jack_flash = app.make_global(0u16);

    // Clear any hanging notes from a prior respawn.
    midi_kick.send_note_off(note_kick).await;
    midi_snare.send_note_off(note_snare).await;
    midi_hats.send_note_off(note_hats).await;

    if muted {
        leds.unset(0, Led::Button);
    } else {
        leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
    }

    let fut_clock = async {
        let mut origin: u32 = 0;
        let mut origin_set = false;
        let mut kick_on = false;
        let mut snare_on = false;
        let mut hats_on = false;
        let mut gate_off_at: Option<u32> = None;
        // Fire-once guard per 16th slot; u32::MAX = nothing fired yet.
        let mut last_fired_slot = u32::MAX;
        // Drum app: GATE % scales a short trigger (1..=TRIG_MAX_TICKS), never a
        // sustained note. Feel/swing only move the attack — they must not stretch
        // the gate into the next 16th (that looked like Chord Vamp holds).
        const TRIG_MAX_TICKS: u32 = 2;
        let gate_len =
            (TRIG_MAX_TICKS as i32 * gatel / 100).clamp(1, TRIG_MAX_TICKS as i32) as u32;

        loop {
            match clock.wait_for_event(ClockDivision::_1).await {
                ClockEvent::Reset | ClockEvent::Stop => {
                    if kick_on {
                        midi_kick.send_note_off(note_kick).await;
                        kick_on = false;
                    }
                    if snare_on {
                        midi_snare.send_note_off(note_snare).await;
                        snare_on = false;
                    }
                    if hats_on {
                        midi_hats.send_note_off(note_hats).await;
                        hats_on = false;
                    }
                    if let Some(ref jack) = out_jack {
                        jack.set_value(pulse_idle(range));
                    }
                    gate_off_at = None;
                    origin_set = false;
                    last_fired_slot = u32::MAX;
                    glob_reset.set(false);
                    leds.unset(0, Led::Top);
                    leds.unset(0, Led::Bottom);
                }
                ClockEvent::Tick => {
                    let clkn = ticks() as u32;

                    if !origin_set || glob_reset.get() {
                        origin = clkn;
                        origin_set = true;
                        last_fired_slot = u32::MAX;
                        glob_reset.set(false);
                    }

                    let pos = clkn.wrapping_sub(origin);
                    // Absolute 16th slot since origin (not wrapped) for the fire-once guard.
                    let slot = pos / SIXTEENTH;
                    let step = (pos / SIXTEENTH) % STEPS_PER_BAR;
                    let phase = pos % SIXTEENTH;
                    let feel_val = if cv_jack == CV_JACK_IN && cv_dest == DEST_FEEL {
                        mod_u16(glob_feel.get(), glob_cv_val.get())
                    } else {
                        glob_feel.get()
                    };
                    let (g_lo, g_hi, g_frac) = genre_pair(glob_genre_fader.get(), NUM_GENRES);
                    let near = genre_nearest(glob_genre_fader.get(), NUM_GENRES);
                    let pat = &PATTERNS[near];
                    let pat_lo = &PATTERNS[g_lo];
                    let pat_hi = &PATTERNS[g_hi];
                    let bias = lerp_u8(swing_bias(g_lo), swing_bias(g_hi), g_frac);
                    let swing_pct = feel_swing_pct(bias, feel_val, glob_swing_max.get());
                    let timing_char = lerp_i32(
                        i32::from(pat_lo.timing[(step % STEPS_PER_BAR) as usize]),
                        i32::from(pat_hi.timing[(step % STEPS_PER_BAR) as usize]),
                        g_frac,
                    );
                    let timing_off = feel_lerp_i32(0, timing_char, feel_val);
                    let delay = ((swing_delay_ticks(step, swing_pct, glob_reversed.get()) as i32)
                        + timing_off)
                        .clamp(0, (SIXTEENTH as i32) - 1) as u32;

                    // Note / jack off
                    if let Some(off_at) = gate_off_at {
                        if clkn >= off_at {
                            if kick_on {
                                midi_kick.send_note_off(note_kick).await;
                                kick_on = false;
                            }
                            if snare_on {
                                midi_snare.send_note_off(note_snare).await;
                                snare_on = false;
                            }
                            if hats_on {
                                midi_hats.send_note_off(note_hats).await;
                                hats_on = false;
                            }
                            if let Some(ref jack) = out_jack {
                                jack.set_value(pulse_idle(range));
                            }
                            gate_off_at = None;
                            leds.set(0, Led::Bottom, led_color, Brightness::Off);
                        }
                    }

                    // Fire-once guard: a feel/density change mid-window
                    // can't skip a step or fire it twice.
                    if slot != last_fired_slot && !glob_muted.get() {
                        let density = if cv_jack == CV_JACK_IN && cv_dest == DEST_DENSITY {
                            mod_u16(glob_density.get(), glob_cv_val.get())
                        } else {
                            glob_density.get()
                        };

                        let kick_core = bit_set(pat.kick, step);
                        let snare_core = bit_set(pat.snare, step);
                        let hats_core = bit_set(pat.hats, step);
                        // `Some(frac)`: this step's fill bit for that voice is
                        // being progressively revealed by the density fader —
                        // frac (0..=255) is how far in it's faded (continuum,
                        // no hard zone jumps).
                        let kick_ghost = fill_reveal(pat.kick_fill, density, step);
                        let snare_ghost = fill_reveal(pat.snare_fill, density, step);
                        let hats_ghost = fill_reveal(pat.hats_fill, density, step);

                        let core_hit = kick_core || snare_core || hats_core;
                        let any_ghost =
                            kick_ghost.is_some() || snare_ghost.is_some() || hats_ghost.is_some();
                        // Ghost-only steps (no core hit) drag a little behind
                        // the grid as density rises — a looser, more human
                        // pocket — but never displace a core hit's timing.
                        // Ghost drag also scales with Feel so low Feel stays straight.
                        let ghost_extra = feel_lerp_u16(0, ghost_drag_ticks(density) as u16, feel_val)
                            as u32;
                        let required_delay = if core_hit || !any_ghost {
                            delay
                        } else {
                            (delay + ghost_extra).min(SIXTEENTH - 1)
                        };

                        if phase >= required_delay {
                            last_fired_slot = slot;

                            let do_kick = kick_core || kick_ghost.is_some();
                            let do_snare = snare_core || snare_ghost.is_some();
                            let do_hats = hats_core || hats_ghost.is_some();

                            // A late-swung previous hit may still be sounding
                            // (its gate-off lands after this step's start):
                            // flush note-offs before re-triggering to avoid
                            // overlapping note-ons on the same key.
                            if (do_kick || do_snare || do_hats) && gate_off_at.is_some() {
                                if kick_on {
                                    midi_kick.send_note_off(note_kick).await;
                                    kick_on = false;
                                }
                                if snare_on {
                                    midi_snare.send_note_off(note_snare).await;
                                    snare_on = false;
                                }
                                if hats_on {
                                    midi_hats.send_note_off(note_hats).await;
                                    hats_on = false;
                                }
                                gate_off_at = None;
                            }

                            if do_kick {
                                let v = match kick_ghost {
                                    Some(frac) if !kick_core => {
                                        let g = ghost_vel_pct(frac, 18, 35);
                                        feel_lerp_u16(FLAT_VEL, g, feel_val)
                                    }
                                    _ => core_vel_pct(
                                        lerp_u8(pat_lo.kick_base, pat_hi.kick_base, g_frac),
                                        lerp_u8(pat_lo.kick_accent, pat_hi.kick_accent, g_frac),
                                        pat.kick_acc_mask,
                                        step,
                                        feel_val,
                                    ),
                                };
                                midi_kick.send_note_on(note_kick, midi_vel(v)).await;
                                kick_on = true;
                            }
                            if do_snare {
                                let v = match snare_ghost {
                                    Some(frac) if !snare_core => {
                                        let g = ghost_vel_pct(frac, 15, 32);
                                        feel_lerp_u16(FLAT_VEL, g, feel_val)
                                    }
                                    _ => core_vel_pct(
                                        lerp_u8(pat_lo.snare_base, pat_hi.snare_base, g_frac),
                                        lerp_u8(pat_lo.snare_accent, pat_hi.snare_accent, g_frac),
                                        pat.snare_acc_mask,
                                        step,
                                        feel_val,
                                    ),
                                };
                                midi_snare.send_note_on(note_snare, midi_vel(v)).await;
                                snare_on = true;
                            }
                            if do_hats {
                                let v = match hats_ghost {
                                    Some(frac) if !hats_core => {
                                        let g = ghost_vel_pct(frac, 12, 28);
                                        feel_lerp_u16(FLAT_VEL, g, feel_val)
                                    }
                                    _ => core_vel_pct(
                                        lerp_u8(pat_lo.hats_base, pat_hi.hats_base, g_frac),
                                        lerp_u8(pat_lo.hats_accent, pat_hi.hats_accent, g_frac),
                                        pat.hats_acc_mask,
                                        step,
                                        feel_val,
                                    ),
                                };
                                midi_hats.send_note_on(note_hats, midi_vel(v)).await;
                                hats_on = true;
                            }

                            if do_kick || do_snare || do_hats {
                                let level = if glob_jack_mode.get() == JACK_STACKED {
                                    stacked_pulse_level(do_kick, do_snare, do_hats, range)
                                } else {
                                    any_pulse_level(do_kick, do_snare, do_hats, range)
                                };
                                if let Some(ref jack) = out_jack {
                                    jack.set_value(level);
                                }
                                // Never hold past this 16th — late Feel/swing
                                // attacks still get a blip, not a smear.
                                let room = SIXTEENTH.saturating_sub(phase).max(1);
                                let pulse = gate_len.min(room);
                                gate_off_at = Some(clkn.wrapping_add(pulse));
                                leds.set(0, Led::Bottom, led_color, Brightness::High);
                            }
                        }
                    }

                    // Top LED: bar progress by default; genre while Shift held;
                    // Feel amount while Button held.
                    match glob_latch_layer.get() {
                        LatchLayer::Main => {
                            leds.set(
                                0,
                                Led::Top,
                                led_color,
                                Brightness::Custom(((step * 255) / STEPS_PER_BAR) as u8),
                            );
                        }
                        LatchLayer::Alt => {
                            let fader_now = faders.get_value();
                            let color = spectrum_color(fader_now);
                            let led = split_unsigned_value(fader_now);
                            leds.set(0, Led::Top, color, Brightness::Custom(led[0]));
                            leds.set(0, Led::Bottom, color, Brightness::Custom(led[1]));
                            if glob_reverse_fade.get() == 0 && glob_jack_flash.get() == 0 {
                                leds.set(0, Led::Button, color, Brightness::High);
                            }
                        }
                        LatchLayer::Third => {
                            let s = glob_feel.get();
                            leds.set(0, Led::Top, Color::Red, Brightness::Custom((s / 16) as u8));
                        }
                    }
                }
                _ => {}
            }
        }
    };

    let fut_buttons = async {
        loop {
            buttons.wait_for_any_down().await;
            if buttons.is_shift_pressed() {
                long_press_fired.set(false);
                buttons.wait_for_up(0).await;
                if !long_press_fired.get() {
                    // Shift + short: reverse swing
                    let reversed = glob_reversed.toggle();
                    storage.modify_and_save(|s| s.reversed = reversed);
                    glob_reverse_fade_up.set(!reversed);
                    glob_reverse_fade.set(REVERSE_FADE_MS);
                }
            } else {
                long_press_fired.set(false);
                glob_fader_moved.set(false);
                glob_fader_at_down.set(faders.get_value());
                buttons.wait_for_up(0).await;
                if !long_press_fired.get() {
                    // Short: reset to downbeat (and not a Feel scrub)
                    if !glob_fader_moved.get() {
                        glob_reset.set(true);
                    }
                } else if !glob_fader_moved.get() {
                    // Long (no fader move): mute
                    let muted = glob_muted.toggle();
                    storage.modify_and_save(|s| s.muted = muted);
                    if muted {
                        leds.unset(0, Led::Button);
                        if let Some(ref jack) = out_jack {
                            jack.set_value(pulse_idle(range));
                        }
                        midi_kick.send_note_off(note_kick).await;
                        midi_snare.send_note_off(note_snare).await;
                        midi_hats.send_note_off(note_hats).await;
                    } else {
                        leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
                    }
                }
            }
        }
    };

    let long_press = async {
        loop {
            let (_, is_shift) = buttons.wait_for_any_long_press().await;
            long_press_fired.set(true);
            if is_shift {
                // Shift + long: toggle CV Out jack activity mode (Any ↔ Stacked)
                let next = if glob_jack_mode.get() == JACK_STACKED {
                    JACK_ANY
                } else {
                    JACK_STACKED
                };
                glob_jack_mode.set(next);
                storage.modify_and_save(|s| s.jack_mode = next);
                glob_jack_flash.set(300);
                if !glob_muted.get() {
                    let color = if next == JACK_STACKED {
                        Color::Violet
                    } else {
                        Color::Yellow
                    };
                    leds.set(0, Led::Button, color, Brightness::High);
                }
            }
            // Plain long: mute is handled on release only if the fader never
            // moved (Button+Fader Feel scrub must not mute).
        }
    };

    let fut_faders = async {
        let mut latch = app.make_latch(faders.get_value());
        loop {
            faders.wait_for_change_at(0).await;
            let fader_val = faders.get_value();
            let latch_layer = glob_latch_layer.get();

            // Button+Feel scrub: any real move cancels mute-on-long / reset-on-tap.
            if buttons.is_button_pressed(0) && !buttons.is_shift_pressed() {
                let delta = fader_val.abs_diff(glob_fader_at_down.get());
                if delta > FADER_MOVE_THRESH {
                    glob_fader_moved.set(true);
                }
            }

            let target_value = match latch_layer {
                LatchLayer::Main => storage.query(|s| s.density),
                LatchLayer::Alt => glob_genre_fader.get(),
                LatchLayer::Third => storage.query(|s| s.feel),
            };

            if let Some(new_value) = latch.update(fader_val, latch_layer, target_value) {
                match latch_layer {
                    LatchLayer::Main => {
                        glob_density.set(new_value);
                        storage.modify_and_save(|s| s.density = new_value);
                    }
                    LatchLayer::Alt => {
                        glob_genre_fader.set(new_value);
                        let g = value_to_index(new_value, NUM_GENRES);
                        if g != glob_genre.get() {
                            glob_genre.set(g);
                            // Do NOT await params.update here — FRAM + MIDI SysEx
                            // would stall the whole fader/latch task (all layers hang).
                            glob_genre_dirty.set(true);
                        }
                    }
                    LatchLayer::Third => {
                        glob_feel.set(new_value);
                        storage.modify_and_save(|s| s.feel = new_value);
                        glob_fader_moved.set(true);
                    }
                }
            }
        }
    };

    let scene_handler = async {
        loop {
            match app.wait_for_scene_event().await {
                SceneEvent::LoadScene(scene) => {
                    storage.load_from_scene(scene).await;
                    let (feel, density, jack_mode, reversed, muted) =
                        storage.query(|s| (s.feel, s.density, s.jack_mode, s.reversed, s.muted));
                    glob_feel.set(feel);
                    glob_density.set(density);
                    glob_jack_mode.set(jack_mode);
                    glob_reversed.set(reversed);
                    glob_muted.set(muted);
                    // Genre lives in params (Configurator); refresh from there.
                    let g = params.query(|p| p.genre.min(NUM_GENRES - 1));
                    glob_genre.set(g);
                    glob_genre_fader.set(genre_fader_center(g, NUM_GENRES));
                    if muted {
                        leds.unset(0, Led::Button);
                    } else {
                        leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
                    }
                }
                SceneEvent::SaveScene(scene) => storage.save_to_scene(scene).await,
            }
        }
    };

    let shift = async {
        let mut prev_gate_high = false;
        loop {
            app.delay_millis(1).await;
            if let Some(ref input) = in_jack {
                let in_val = attenuate_bipolar(input.get_value(), cv_att);
                glob_cv_val.set(in_val);
                if cv_dest == DEST_RESET {
                    let high = in_val >= TRIG_HIGH;
                    if high && !prev_gate_high {
                        glob_reset.set(true);
                    }
                    prev_gate_high = high;
                } else {
                    prev_gate_high = false;
                }
            }
            // While holding for Feel (Third), poll fader vs press-down so a
            // scrub cancels mute even if wait_for_change races the release.
            if buttons.is_button_pressed(0) && !buttons.is_shift_pressed() {
                let delta = faders.get_value().abs_diff(glob_fader_at_down.get());
                if delta > FADER_MOVE_THRESH {
                    glob_fader_moved.set(true);
                }
            }

            let latch_active_layer = if buttons.is_shift_pressed() && !buttons.is_button_pressed(0)
            {
                LatchLayer::Alt
            } else if !buttons.is_shift_pressed() && buttons.is_button_pressed(0) {
                LatchLayer::Third
            } else {
                LatchLayer::Main
            };
            glob_latch_layer.set(latch_active_layer);

            // Reverse fade overrides button LED
            let fade_left = glob_reverse_fade.get();
            if fade_left > 0 {
                let elapsed = REVERSE_FADE_MS.saturating_sub(fade_left);
                let bright = if glob_reverse_fade_up.get() {
                    ((elapsed as u32 * 255) / REVERSE_FADE_MS as u32) as u8
                } else {
                    (((REVERSE_FADE_MS - elapsed) as u32 * 255) / REVERSE_FADE_MS as u32) as u8
                };
                leds.set(0, Led::Button, Color::White, Brightness::Custom(bright));
                glob_reverse_fade.set(fade_left.saturating_sub(1));
                if fade_left == 1 {
                    // Don't leave the LED stuck white when muted.
                    if glob_muted.get() {
                        leds.unset(0, Led::Button);
                    } else {
                        leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
                    }
                }
            }

            // Jack-mode flash counts down independently of the reverse fade so it
            // can't stall behind it; the restore is skipped while a fade is
            // still animating (the fade's own end handler restores the LED).
            let flash_left = glob_jack_flash.get();
            if flash_left > 0 {
                let left = flash_left.saturating_sub(1);
                glob_jack_flash.set(left);
                if left == 0 && glob_reverse_fade.get() == 0 {
                    if glob_muted.get() {
                        leds.unset(0, Led::Button);
                    } else {
                        leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
                    }
                }
            }
        }
    };

    // Persist genre after Shift scrub without blocking faders.
    let genre_persist = async {
        loop {
            app.delay_millis(40).await;
            // Only flush while Shift is up so scrub stays responsive.
            let shifting = buttons.is_shift_pressed() && !buttons.is_button_pressed(0);
            if glob_genre_dirty.get() && !shifting {
                glob_genre_dirty.set(false);
                let g = glob_genre.get().min(NUM_GENRES - 1);
                params.update(|p| p.genre = g).await;
            }
        }
    };

    join(
        join5(fut_clock, fut_buttons, fut_faders, scene_handler, shift),
        join(long_press, genre_persist),
    )
    .await;
}
