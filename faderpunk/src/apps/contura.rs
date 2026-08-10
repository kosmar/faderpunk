//! Contura — melodic contour over selectable 12-TET pitch-class sets.
//!
//! Generates phrases with mixed note lengths. Each scale set carries a compact
//! melodic feel (contour, leap, density bias, sustain, tonic pull, ornaments).
//! Faders: Main = interval, Alt = phrase length, Third = note density.
//! Labels and feels are conventional interval-pattern flavors — not claims
//! about living musical practice. Optional follow of device tonic / scale.

use embassy_futures::{
    join::{join3, join5},
    select::{select, select3},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use heapless::Vec;
use midly::num::u7;
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue, latch::LatchLayer, quantizer::Pitch, AppIcon, Brightness, Color, Config, Key,
    MidiChannel, MidiNote, MidiOut, Note, Param, Range, Value, VoltPerOct, APP_MAX_PARAMS,
};

use crate::{
    app::{App, AppParams, AppStorage, Die, Led, ManagedStorage, ParamStore, SceneEvent},
    tasks::global_config::get_global_config,
};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 10;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
const OCTAVE_BLINK_MS: u16 = 250;
const BUTTON_DUCK_MS: u16 = 25;

const MIN_PHRASE: u8 = 3;
const MAX_PHRASE: u8 = 28;
const POOL_CAP: usize = 48;

/// Clock divisions (24 PPQN ticks). Index matches Division param.
/// Labels are bar-relative in 4/4 (bar = 96 ticks): 1/1 = whole bar, etc.
/// One tick is the floor: a straight 1/64 would need 1.5 ticks, so the finest
/// step the internal clock can express is the 1/64 triplet.
const RESOLUTION: [u32; 13] = [384, 192, 96, 48, 24, 16, 12, 8, 6, 4, 3, 2, 1];
const DIV_LABELS: &[&str] = &[
    "4/1", "2/1", "1/1", "1/2", "1/4", "1/4T", "1/8", "1/8T", "1/16", "1/16T", "1/32", "1/32T",
    "1/64T",
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

/// Melodic biases keyed to each scale label.
/// Stylistic 12-TET contour flavors only — not ethnographic claims.
#[derive(Clone, Copy)]
struct ScaleFeel {
    /// Added to density before rest roll (higher → fewer rests).
    density_bias: i16,
    /// Scales max interval step (128 = ×1.0).
    leap_q8: u16,
    /// Shifts long-note bias in duration picks.
    sustain_bias: i16,
    /// Chance (0..=4095) to snap toward a tonic pitch-class.
    tonic_pull: u16,
    /// 0 arch · 1 descend-heavy · 2 undulate · 3 plateau/neighbor.
    contour: u8,
    /// Extra chance (0..=4095) to force a single-degree step.
    step_glue: u16,
    /// Chance (0..=4095) of a neighbor hop before settling.
    ornament: u16,
}

const FEEL_NEUTRAL: ScaleFeel = ScaleFeel {
    density_bias: 0,
    leap_q8: 128,
    sustain_bias: 0,
    tonic_pull: 400,
    contour: 0,
    step_glue: 800,
    ornament: 200,
};

/// Parallel to SCALE_LABELS / SCALE_MASKS.
const SCALE_FEELS: [ScaleFeel; SCALE_COUNT] = [
    // Ionian — balanced arch, mild tonic gravity
    ScaleFeel {
        density_bias: 200,
        leap_q8: 128,
        sustain_bias: 0,
        tonic_pull: 700,
        contour: 0,
        step_glue: 900,
        ornament: 250,
    },
    // Dorian — stepwise, gentle undulation
    ScaleFeel {
        density_bias: 150,
        leap_q8: 110,
        sustain_bias: 200,
        tonic_pull: 500,
        contour: 2,
        step_glue: 1200,
        ornament: 350,
    },
    // Phrygian — descend-heavy, narrower motion
    ScaleFeel {
        density_bias: -100,
        leap_q8: 96,
        sustain_bias: 150,
        tonic_pull: 900,
        contour: 1,
        step_glue: 1400,
        ornament: 400,
    },
    // Mixolydian — brighter, more leaps, denser
    ScaleFeel {
        density_bias: 400,
        leap_q8: 150,
        sustain_bias: -200,
        tonic_pull: 450,
        contour: 0,
        step_glue: 600,
        ornament: 300,
    },
    // Aeolian — longer tones, descending bias
    ScaleFeel {
        density_bias: -50,
        leap_q8: 112,
        sustain_bias: 450,
        tonic_pull: 850,
        contour: 1,
        step_glue: 1100,
        ornament: 280,
    },
    // Pent Maj — sticky steps, lively density
    ScaleFeel {
        density_bias: 350,
        leap_q8: 90,
        sustain_bias: -150,
        tonic_pull: 600,
        contour: 0,
        step_glue: 1800,
        ornament: 200,
    },
    // Pent Min — stepwise with downward lean
    ScaleFeel {
        density_bias: 250,
        leap_q8: 92,
        sustain_bias: 100,
        tonic_pull: 650,
        contour: 1,
        step_glue: 1700,
        ornament: 220,
    },
    // Blues Min — leaps, rests, neighbor turns
    ScaleFeel {
        density_bias: -250,
        leap_q8: 170,
        sustain_bias: -100,
        tonic_pull: 550,
        contour: 2,
        step_glue: 500,
        ornament: 900,
    },
    // In Sen — sparse, sustained, narrow
    ScaleFeel {
        density_bias: -450,
        leap_q8: 80,
        sustain_bias: 700,
        tonic_pull: 1000,
        contour: 1,
        step_glue: 1600,
        ornament: 500,
    },
    // Yo — open rising pent flavor
    ScaleFeel {
        density_bias: 300,
        leap_q8: 100,
        sustain_bias: -50,
        tonic_pull: 500,
        contour: 0,
        step_glue: 1500,
        ornament: 250,
    },
    // Hirajoshi — sparse ornaments, plateau
    ScaleFeel {
        density_bias: -350,
        leap_q8: 85,
        sustain_bias: 550,
        tonic_pull: 800,
        contour: 3,
        step_glue: 1500,
        ornament: 750,
    },
    // Bhairav — sustained, strong tonic, occasional leap
    ScaleFeel {
        density_bias: -150,
        leap_q8: 140,
        sustain_bias: 650,
        tonic_pull: 1400,
        contour: 3,
        step_glue: 1000,
        ornament: 450,
    },
    // Kafi — dorian-adjacent undulation
    ScaleFeel {
        density_bias: 100,
        leap_q8: 115,
        sustain_bias: 250,
        tonic_pull: 600,
        contour: 2,
        step_glue: 1100,
        ornament: 400,
    },
    // Bhupali — bright pent ascent
    ScaleFeel {
        density_bias: 320,
        leap_q8: 95,
        sustain_bias: -100,
        tonic_pull: 550,
        contour: 0,
        step_glue: 1600,
        ornament: 200,
    },
    // Hijaz — dramatic leaps, then glue
    ScaleFeel {
        density_bias: -100,
        leap_q8: 185,
        sustain_bias: 300,
        tonic_pull: 1100,
        contour: 1,
        step_glue: 700,
        ornament: 650,
    },
    // Bayati — descending with ornaments
    ScaleFeel {
        density_bias: 0,
        leap_q8: 105,
        sustain_bias: 200,
        tonic_pull: 900,
        contour: 1,
        step_glue: 1300,
        ornament: 700,
    },
    // Rast — balanced, sustained, tonic-centered
    ScaleFeel {
        density_bias: 150,
        leap_q8: 120,
        sustain_bias: 500,
        tonic_pull: 1200,
        contour: 0,
        step_glue: 1000,
        ornament: 350,
    },
    // Gamelan — ostinato neighbors, even short tones
    ScaleFeel {
        density_bias: 500,
        leap_q8: 70,
        sustain_bias: -400,
        tonic_pull: 300,
        contour: 3,
        step_glue: 2200,
        ornament: 1100,
    },
    // Hungarian — wide leaps, held peaks
    ScaleFeel {
        density_bias: -200,
        leap_q8: 200,
        sustain_bias: 600,
        tonic_pull: 750,
        contour: 0,
        step_glue: 400,
        ornament: 500,
    },
    // Folk — dance density, mixed leaps
    ScaleFeel {
        density_bias: 450,
        leap_q8: 155,
        sustain_bias: -250,
        tonic_pull: 500,
        contour: 2,
        step_glue: 700,
        ornament: 400,
    },
];

fn scale_feel(follow_scale: bool, scale_set: usize) -> ScaleFeel {
    if follow_scale {
        FEEL_NEUTRAL
    } else {
        SCALE_FEELS[scale_set.min(SCALE_COUNT - 1)]
    }
}

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
    variants: SCALE_LABELS,
})
.add_param(Param::Enum {
    name: "Division",
    variants: DIV_LABELS,
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
            midi_out: MidiOut([true, false, false]), // USB only — all-ports floods DIN+USB
            range: Range::_0_10V,
            vpo: VoltPerOct::Standard,
            follow_tonic: true,
            follow_scale: false,
            scale_set: 0, // Ionian
            division: 6,  // 1/8 — quieter default on crowded playground USB
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
    /// Shift fader: phrase length only (steps).
    phrase_saved: u16,
    /// Button+fader: note density (rests ↔ continuous).
    density_saved: u16,
    scale_set: u8,
    octaves: u8,
    muted: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            interval_saved: 2048,
            phrase_saved: 2048,
            density_saved: 1800,
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

/// Scale list wraps in both directions (Ionian ↔ Folk).
fn wrap_scale(i: isize) -> u8 {
    let n = SCALE_COUNT as isize;
    (((i % n) + n) % n) as u8
}

fn next_scale(cur: u8) -> u8 {
    wrap_scale(cur as isize + 1)
}

fn prev_scale(cur: u8) -> u8 {
    wrap_scale(cur as isize - 1)
}

fn clamp_scale(s: u8) -> u8 {
    (s as usize).min(SCALE_COUNT - 1) as u8
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

fn follow_mask_tonic(
    follow_scale: bool,
    follow_tonic: bool,
    scale_set: usize,
    base: MidiNote,
) -> (u16, u8) {
    if follow_scale || follow_tonic {
        let c = get_global_config();
        let mask = if follow_scale {
            let key = c.quantizer.key;
            if key == Key::Off {
                Key::Chromatic.as_u16_key()
            } else {
                key.as_u16_key()
            }
        } else {
            SCALE_MASKS[scale_set.min(SCALE_COUNT - 1)]
        };
        let tonic = if follow_tonic {
            c.quantizer.tonic as u8
        } else {
            midi_u8(base) % 12
        };
        (mask, tonic)
    } else {
        (
            SCALE_MASKS[scale_set.min(SCALE_COUNT - 1)],
            midi_u8(base) % 12,
        )
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
    // Wide audible span: short motifs ↔ long arcs.
    let span = (MAX_PHRASE - MIN_PHRASE) as u32;
    (MIN_PHRASE as u32 + (v as u32 * span) / 4095) as u8
}

/// Third fader → rest probability gate (higher = fewer rests).
/// Quadratic curve so the top half clearly densifies.
fn density_from_fader(v: u16) -> u16 {
    let t = v as u32;
    let curved = (t * t) / 4095;
    (350 + (curved * 3700) / 4095) as u16
}

/// Main fader → max scale-degree step. Bottom sticks to steps of 1;
/// top opens wide leaps (quadratic).
fn max_step_from_fader(v: u16, pool_len: usize) -> usize {
    let max = (pool_len / 2).clamp(1, 12);
    let t = v as u32;
    let curved = (t * t) / 4095;
    1 + ((curved as usize * max.saturating_sub(1)) / 4095)
}

/// Expressivity is no longer a fader — ScaleFeel + Main interval drive it.
fn express_from_feel_and_interval(feel: ScaleFeel, interval: u16) -> u16 {
    let leap = (feel.leap_q8 as i32 - 128) * 10;
    let from_main = (interval as i32 - 2048) / 2;
    (2048 + leap + feel.sustain_bias as i32 + from_main).clamp(0, 4095) as u16
}

fn pick_duration(die: &Die, express: u16, remain: u8, feel: ScaleFeel, min_dur: u8) -> u8 {
    // One RNG call — keep Contura's clock step short.
    let roll = die.roll();
    let long_bias = (express as i32 + feel.sustain_bias as i32).clamp(0, 4095) as u16;
    let short_gate = 1200u32 + (4095u32 - long_bias as u32) / 2;
    let mid_gate = 2800u32.saturating_add_signed(feel.sustain_bias as i32 / 2);
    let dur = if (roll as u32) < short_gate {
        min_dur
    } else if (roll as u32) < mid_gate {
        (2 + ((roll % 3) as u8)).max(min_dur)
    } else {
        (remain / 2).max(3).min(remain.max(1)).max(min_dur)
    };
    // At the end of a phrase `remain` can drop below `min_dur` (fine grids ask
    // for 3–4 slots). The note has to fit the phrase, so the floor yields —
    // clamp(lo, hi) panics when lo > hi.
    let hi = remain.max(1);
    let lo = min_dur.max(1).min(hi);
    dur.clamp(lo, hi)
}

/// At fine grids, force longer holds so we don't fire a note every division step.
fn min_duration_for_div(div: u32) -> u8 {
    match div {
        0..=2 => 4,
        3..=4 => 3,
        5..=8 => 2,
        // 1/16T…1/32T — hold several slots; Contura must stay cheap under load.
        _ => 3,
    }
}

/// Soft MIDI gap (ms) — finer divisions need more spacing on the shared USB pipe.
fn midi_gap_ms_for_div(div: u32) -> u16 {
    match div {
        0..=4 => 24,
        5..=8 => 20,
        9..=16 => 18,
        _ => 16,
    }
}

fn shaped_max_step(interval: u16, pool_len: usize, feel: ScaleFeel) -> usize {
    let base = max_step_from_fader(interval, pool_len);
    let scaled = ((base as u32 * feel.leap_q8 as u32) / 128).max(1) as usize;
    scaled.clamp(1, (pool_len / 2).clamp(1, 10))
}

fn contour_rising(contour: u8, phrase_step: u8, phrase_len: u8) -> bool {
    let len = phrase_len.max(1);
    match contour {
        1 => phrase_step < len / 4,
        2 => (phrase_step / 2).is_multiple_of(2),
        3 => phrase_step < len / 3,
        _ => phrase_step < len.saturating_add(1) / 2,
    }
}

fn nearest_tonic_index(pool: &[u8], cur: usize, tonic: u8) -> usize {
    if pool.is_empty() {
        return 0;
    }
    let tonic = tonic % 12;
    let cur = cur.min(pool.len() - 1);
    let mut best = cur;
    let mut best_dist = usize::MAX;
    for (i, &n) in pool.iter().enumerate() {
        if n % 12 != tonic {
            continue;
        }
        let dist = i.abs_diff(cur);
        if dist < best_dist {
            best_dist = dist;
            best = i;
        }
    }
    best
}

fn pick_next_index(
    die: &Die,
    cur: usize,
    pool_len: usize,
    max_step: usize,
    express: u16,
    rising: bool,
    feel: ScaleFeel,
) -> usize {
    if pool_len <= 1 {
        return 0;
    }
    // Single roll sliced into decisions — was up to 6 rolls and starved CLOCK_PUBSUB.
    let r = die.roll();
    let repeat_chance = 4095u16.saturating_sub(express) / 3;
    if r < repeat_chance {
        return cur;
    }

    let mut step = 1 + ((r as usize >> 2) % max_step.max(1));
    if (r & 0x3ff) < feel.step_glue {
        step = 1;
    }

    let mut signed = if rising { step as i16 } else { -(step as i16) };
    if express > 2800 && (r & 0xfff) < 600 {
        signed = -signed;
    }
    if (r >> 1) < feel.ornament {
        signed = if (r & 1) == 0 { 1 } else { -1 };
    }

    let next = cur as i16 + signed;
    next.clamp(0, pool_len as i16 - 1) as usize
}

/// One Contura instance is enough; large future + dense layouts stress the arena.
#[embassy_executor::task(pool_size = 4)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(app.app_id, app.layout_id, Params::default());
    let storage = ManagedStorage::<Storage>::new(app.app_id, app.layout_id);

    param_store.load().await;

    storage.load().await;
    // Sync param→scene scale before saver_task runs in parallel (RefCell).
    let scale_init = clamp_scale(param_store.query(|p| p.scale_set as u8));
    storage.modify(|s| s.scale_set = scale_init);

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
    // Ticker only — never subscribe to CLOCK_PUBSUB. A lagged subscriber fills the
    // gatekeeper queue and stalls the whole device clock (worse at fine divisions).
    let ticks = app.clock_ticker();
    let die = app.use_die();
    let midi = app.use_midi_output(midi_out, midi_chan, false);
    let cv = app.make_out_jack(0, range).await;

    let (interval0, phrase0, density0, _scale0, octaves0, muted0) = storage.query(|s| {
        (
            s.interval_saved,
            s.phrase_saved,
            s.density_saved,
            s.scale_set,
            s.octaves,
            s.muted,
        )
    });

    let scale_init = clamp_scale(scale_param as u8);

    let glob_interval = app.make_global(interval0);
    let glob_phrase = app.make_global(phrase0);
    let glob_density = app.make_global(density0);
    let glob_div = app.make_global(RESOLUTION[division.min(RESOLUTION.len() - 1)]);
    let glob_scale = app.make_global(scale_init);
    let glob_octaves = app.make_global(clamp_octaves(octaves0));
    let glob_muted = app.make_global(muted0);
    let glob_latch = app.make_global(LatchLayer::Main);
    let glob_fader_moved = app.make_global(false);
    let glob_octave_blink = app.make_global(0u16);
    let glob_button_duck = app.make_global(0u16);
    let long_press_fired = app.make_global(false);
    let glob_shift_chord = app.make_global(false);
    // Scale/octave change: clock path note-offs before pool rebuild.
    let glob_resets_voice = app.make_global(false);
    let glob_scale_dirty = app.make_global(false);
    let glob_fader_dirty = app.make_global(false);

    if muted0 {
        leds.unset(0, Led::Button);
    } else {
        leds.set(0, Led::Button, set_color(scale_init), LED_BRIGHTNESS);
    }

    // Ticker path owns MIDI/CV directly (no CLOCK_PUBSUB) — one loop avoids
    // pending-flag races and halves the 1 ms wakeups that starved Core 1.
    let glob_silence_req = app.make_global(false);
    let glob_gate_on = app.make_global(false);

    let fut_engine = async {
        let mut pool: Vec<u8, POOL_CAP> = Vec::new();
        let mut phrase_step: u8 = 0;
        let mut remain: u8 = 0;
        let mut gated = false;
        let mut note_on: Option<u8> = None;
        let mut midi_quiet_ms: u16 = 0;
        let mut cached_tonic = 0u8;
        let rebuild = |pool: &mut Vec<u8, POOL_CAP>,
                       cached_tonic: &mut u8,
                       scale_set: u8,
                       octaves: u8|
         -> usize {
            let (mask, tonic) =
                follow_mask_tonic(follow_scale, follow_tonic, scale_set as usize, base_note);
            *cached_tonic = tonic;
            let base = midi_u8(base_note);
            *pool = build_pool(mask, tonic, base, octaves);
            pool.len()
        };

        let mut last_scale = glob_scale.get();
        let mut last_oct = glob_octaves.get();
        let plen0 = rebuild(&mut pool, &mut cached_tonic, last_scale, last_oct);
        let mut idx = plen0 / 3;

        let mut last_seen = ticks();
        let mut last_div_fire: u64 = u64::MAX;
        let mut stall_ms = 0u16;

        let silence = |note_on: &mut Option<u8>,
                       gated: &mut bool,
                       remain: &mut u8,
                       phrase_step: &mut u8| {
            if let Some(n) = note_on.take() {
                
                midi.try_send_note_off(MidiNote::from(n));
            
            }
            
            cv.set_value(0);
        
            *gated = false;
            *remain = 0;
            *phrase_step = 0;
            glob_gate_on.set(false);
        };

        loop {
            // 2 ms: still tracks 1/8–1/32 boundaries; half the timer pressure.
            app.delay_millis(2).await;
            midi_quiet_ms = midi_quiet_ms.saturating_sub(2);

            if glob_silence_req.get() {
                glob_silence_req.set(false);
                silence(&mut note_on, &mut gated, &mut remain, &mut phrase_step);
            }

            let t = ticks();
            if t == last_seen {
                stall_ms = stall_ms.saturating_add(2);
                if stall_ms >= 250 && gated {
                    silence(&mut note_on, &mut gated, &mut remain, &mut phrase_step);
                }
                continue;
            }
            stall_ms = 0;

            if t < last_seen {
                silence(&mut note_on, &mut gated, &mut remain, &mut phrase_step);
                last_seen = t;
                last_div_fire = u64::MAX;
                continue;
            }

            let div = glob_div.get().max(1) as u64;
            let boundary = t - (t % div);
            last_seen = t;
            if boundary == 0 && t < div {
                continue;
            }
            if boundary == last_div_fire {
                continue;
            }
            last_div_fire = boundary;

            let muted = glob_muted.get();
            let scale_set = glob_scale.get();
            let octaves = glob_octaves.get();
            let interval = glob_interval.get();
            let phrase_f = glob_phrase.get();
            let density_f = glob_density.get();

            if scale_set != last_scale || octaves != last_oct || glob_resets_voice.get() {
                glob_resets_voice.set(false);
                if gated {
                    if let Some(n) = note_on.take() {
                        
                        midi.try_send_note_off(MidiNote::from(n));
                    
                    }
                    gated = false;
                    glob_gate_on.set(false);
                }
                remain = 0;
                let plen = rebuild(&mut pool, &mut cached_tonic, scale_set, octaves);
                last_scale = scale_set;
                last_oct = octaves;
                idx = (plen / 3).min(plen.saturating_sub(1));
                phrase_step = 0;
            }
            let plen = pool.len();
            if plen == 0 {
                continue;
            }
            let feel = scale_feel(follow_scale, scale_set as usize);
            let phrase_len = phrase_from_fader(phrase_f).max(1);
            let density = (density_from_fader(density_f) as i32 + feel.density_bias as i32)
                .clamp(200, 4090) as u16;
            let max_step = shaped_max_step(interval, plen, feel);
            let express = express_from_feel_and_interval(feel, interval);
            idx = idx.min(plen - 1);

            if muted {
                if gated {
                    if let Some(n) = note_on.take() {
                        
                        midi.try_send_note_off(MidiNote::from(n));
                    
                    }
                    gated = false;
                }
                remain = 0;
                glob_gate_on.set(false);
                continue;
            }

            let rising = contour_rising(feel.contour, phrase_step, phrase_len);

            if remain > 0 {
                remain -= 1;
                if remain == 0 && gated {
                    if let Some(n) = note_on.take() {
                        
                        midi.try_send_note_off(MidiNote::from(n));
                    
                    }
                    gated = false;
                    glob_gate_on.set(false);
                }
            } else {
                let r = die.roll();
                if r > density {
                    if gated {
                        if let Some(n) = note_on.take() {
                            
                            midi.try_send_note_off(MidiNote::from(n));
                        
                        }
                        gated = false;
                        glob_gate_on.set(false);
                    }
                    remain = 1;
                } else {
                    let steps_left = phrase_len.saturating_sub(phrase_step).max(1);
                    // Cached at rebuild / phrase wrap — avoid GlobalConfig copy per note.
                    let tonic_pc = cached_tonic;
                    if phrase_step == 0 || steps_left <= 2 {
                        if r < 2800 {
                            idx = idx.saturating_sub(max_step.min(idx));
                        }
                        if (r & 0xfff) < feel.tonic_pull {
                            idx = nearest_tonic_index(pool.as_slice(), idx, tonic_pc);
                        }
                    } else {
                        idx = pick_next_index(&die, idx, plen, max_step, express, rising, feel);
                        if (r >> 2) < feel.tonic_pull / 2 {
                            idx = nearest_tonic_index(pool.as_slice(), idx, tonic_pc);
                        }
                    }

                    remain = pick_duration(
                        &die,
                        express,
                        steps_left,
                        feel,
                        min_duration_for_div(div as u32),
                    )
                    .max(1);
                    if let Some(&note) = pool.get(idx.min(plen - 1)) {
                        let pitch_changed = note_on != Some(note);
                        
                        cv.set_value(note_to_pitch(note).as_counts(range, vpo));
                    
                        let gap = midi_gap_ms_for_div(div as u32);
                        if pitch_changed || midi_quiet_ms == 0 {
                            
                            if let Some(old) = note_on {
                                if old != note {
                                    midi.try_send_note_off(MidiNote::from(old));
                                }
                            }
                            midi.try_send_note_on(MidiNote::from(note), 3200);
                        
                            midi_quiet_ms = gap;
                        }
                        note_on = Some(note);
                        gated = true;
                        glob_gate_on.set(true);
                        // Button duck doubles as activity cue.
                        glob_button_duck.set(BUTTON_DUCK_MS);
                    }
                }
            }

            phrase_step = phrase_step.wrapping_add(1);
            if phrase_step >= phrase_len {
                phrase_step = 0;
                let (_, tonic) =
                    follow_mask_tonic(follow_scale, follow_tonic, scale_set as usize, base_note);
                cached_tonic = tonic;
            }
        }
    };

    let fut_faders = async {
        let mut latch = app.make_latch(faders.get_value());
        loop {
            faders.wait_for_change_at(0).await;
            let layer = glob_latch.get();
            // Any layer scrub cancels mute/scale short-press on button release.
            glob_fader_moved.set(true);

            let target = match layer {
                LatchLayer::Main => glob_interval.get(),
                LatchLayer::Alt => glob_phrase.get(),
                LatchLayer::Third => glob_density.get(),
            };

            if let Some(v) = latch.update(faders.get_value(), layer, target) {
                match layer {
                    LatchLayer::Main => {
                        glob_interval.set(v);
                    }
                    LatchLayer::Alt => {
                        glob_phrase.set(v);
                    }
                    LatchLayer::Third => {
                        glob_density.set(v);
                    }
                }
                // Globals only here — never storage.modify while saver may
                // borrow_mut (RefCell panic on Shift+fader scrub).
                glob_fader_dirty.set(true);
            }
        }
    };

    let fut_buttons = async {
        loop {
            let (_, down_shift) = buttons.wait_for_any_down().await;
            let shift_chord = down_shift || buttons.is_shift_pressed();
            glob_shift_chord.set(shift_chord);
            long_press_fired.set(false);
            glob_fader_moved.set(false);
            buttons.wait_for_up(0).await;
            glob_shift_chord.set(false);

            if long_press_fired.get() {
                continue;
            }

            if shift_chord {
                // Shift+short: octave span 1→2→3→4. Long/Shift+Long stay the
                // forward/backward pair, so the odd one out lives here.
                let oct = cycle_octaves(glob_octaves.get());
                glob_octaves.set(oct);
                glob_resets_voice.set(true);
                glob_fader_dirty.set(true);
                leds.set(
                    0,
                    Led::Top,
                    OCT_COLORS[(oct - 1) as usize],
                    Brightness::High,
                );
                glob_octave_blink.set(OCTAVE_BLINK_MS);
            } else if !glob_fader_moved.get() {
                let muted = glob_muted.toggle();
                glob_fader_dirty.set(true);
                if muted {
                    glob_silence_req.set(true);
                    leds.unset(0, Led::Button);
                    leds.unset(0, Led::Top);
                    leds.unset(0, Led::Bottom);
                } else {
                    leds.set(0, Led::Button, set_color(glob_scale.get()), LED_BRIGHTNESS);
                }
            }
        }
    };

    let fut_long = async {
        loop {
            let (_, is_shift_now) = buttons.wait_for_any_long_press().await;
            long_press_fired.set(true);
            let shift_chord = glob_shift_chord.get() || is_shift_now || buttons.is_shift_pressed();

            if shift_chord {
                // Shift+long: previous scale set (wraps Ionian → Folk).
                let prev = prev_scale(glob_scale.get());
                glob_scale.set(prev);
                glob_resets_voice.set(true);
                glob_scale_dirty.set(true);
                leds.set(0, Led::Button, set_color(prev), Brightness::High);
            } else if !glob_fader_moved.get() {
                // Long: next scale set (wraps Folk → Ionian).
                let next = next_scale(glob_scale.get());
                glob_scale.set(next);
                glob_resets_voice.set(true);
                glob_scale_dirty.set(true);
                leds.set(0, Led::Button, set_color(next), Brightness::High);
            }
        }
    };

    let fut_scale_persist = async {
        loop {
            app.delay_millis(400).await;
            let scale_dirty = glob_scale_dirty.get();
            let fader_dirty = glob_fader_dirty.get();
            if !scale_dirty && !fader_dirty {
                continue;
            }
            glob_scale_dirty.set(false);
            glob_fader_dirty.set(false);
            // Single writer for ManagedStorage — avoids RefCell panic when
            // Alt/Third fader scrub races the FRAM debounce task.
            storage.modify_and_save(|st| {
                st.interval_saved = glob_interval.get();
                st.phrase_saved = glob_phrase.get();
                st.density_saved = glob_density.get();
                st.scale_set = clamp_scale(glob_scale.get());
                st.octaves = clamp_octaves(glob_octaves.get());
                st.muted = glob_muted.get();
            });
        }
    };

    let fut_leds = async {
        let mut last_layer = LatchLayer::Main;
        let mut last_gate = false;
        loop {
            app.delay_millis(8).await;

            let layer = if buttons.is_shift_pressed() && !buttons.is_button_pressed(0) {
                LatchLayer::Alt
            } else if !buttons.is_shift_pressed() && buttons.is_button_pressed(0) {
                LatchLayer::Third
            } else {
                LatchLayer::Main
            };
            glob_latch.set(layer);

            if glob_octave_blink.get() > 0 {
                let left = glob_octave_blink.get().saturating_sub(8);
                glob_octave_blink.set(left);
                if left == 0 {
                    leds.unset(0, Led::Top);
                }
            }
            if glob_button_duck.get() > 0 {
                let left = glob_button_duck.get().saturating_sub(8);
                glob_button_duck.set(left);
                if !glob_muted.get() {
                    let bright = if left > 0 {
                        Brightness::Low
                    } else {
                        LED_BRIGHTNESS
                    };
                    leds.set(0, Led::Button, set_color(glob_scale.get()), bright);
                }
            }

            let gate = glob_gate_on.get();
            if layer != last_layer {
                match layer {
                    LatchLayer::Alt => {
                        leds.set(0, Led::Bottom, Color::White, Brightness::Low);
                    }
                    LatchLayer::Third => {
                        leds.set(0, Led::Bottom, set_color(glob_scale.get()), Brightness::Low);
                    }
                    LatchLayer::Main => {
                        if !gate {
                            leds.unset(0, Led::Bottom);
                        }
                    }
                }
            }
            if layer == LatchLayer::Main && gate != last_gate {
                if gate {
                    leds.set(0, Led::Top, led_color, Brightness::Mid);
                } else {
                    leds.unset(0, Led::Top);
                }
            }
            last_gate = gate;
            last_layer = layer;
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
                            st.density_saved,
                            st.scale_set,
                            st.octaves,
                            st.muted,
                        )
                    });
                    glob_interval.set(i);
                    glob_phrase.set(p);
                    glob_density.set(e);
                    glob_scale.set(clamp_scale(s));
                    glob_octaves.set(clamp_octaves(o));
                    glob_muted.set(m);
                    glob_resets_voice.set(true);
                    let div = params.query(|p| p.division);
                    glob_div.set(RESOLUTION[div.min(RESOLUTION.len() - 1)]);
                }
                SceneEvent::SaveScene(_) => {}
            }
        }
    };

    join5(
        fut_engine,
        fut_faders,
        join3(fut_buttons, fut_long, fut_scale_persist),
        fut_leds,
        fut_scene,
    )
    .await;
}
