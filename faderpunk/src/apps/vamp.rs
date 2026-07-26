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
    utils::{attenuate_bipolar, split_unsigned_value, value_to_index},
    AppIcon, Brightness, ClockDivision, Color, Config, Key, MidiCc, MidiChannel, MidiNote, MidiOut,
    Note, Param, Range, Value, VoltPerOct, APP_MAX_PARAMS,
};

use crate::app::{
    App, AppParams, AppStorage, ClockEvent, Die, Led, ManagedStorage, ParamStore, SceneEvent,
};
use crate::apps::genre_palette::{genre_fader_center, GENRE_COLORS, GENRE_NAMES, NUM_GENRES};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 16;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
const VAMP_CAP: usize = 32;
const RING_CAP: usize = 64;
const SOUNDING_CAP: usize = 8;
const TICKS_PER_BAR: u32 = 96;
/// Capture snaps chord *starts* to this grid (16ths at 24 PPQN); hold length stays free.
const CAPTURE_QUANT_TICKS: u32 = 6;
const NUM_DEGREES: usize = 7;
/// Auto / genre-seed progression length (classic vamp loop).
const NUM_SLOTS: usize = 8;
/// Perform maps unique genre degrees across this many octaves (ascending).
const PERFORM_OCTAVES: usize = 3;
/// Max perform palette size (all 7 degrees × 3 octaves).
const PERFORM_CAP: usize = NUM_DEGREES * PERFORM_OCTAVES;
/// Shift+Fader index for the Capture clip (after the 8 genres).
const CAPTURE_SLOT: usize = NUM_GENRES;
const CAPTURE_COLOR: Color = Color::Rose;

const CV_JACK_OUT: usize = 0;
const CV_JACK_IN: usize = 1;

const CAPTURE_NAMES: &[&str] = &["16 bars", "8 bars", "4 bars", "2 bars", "1 bar"];
/// Bars for each Capture length enum index.
const CAPTURE_BARS: [u32; 5] = [16, 8, 4, 2, 1];

const SCALE_NAMES: &[&str] = &[
    "Ionian",
    "Dorian",
    "Phrygian",
    "Lydian",
    "Mixolydian",
    "Aeolian",
    "Locrian",
    "Pent Maj",
    "Pent Min",
];

const VOICING_NAMES: &[&str] = &["Triads", "7ths", "9ths", "Spread"];
const AUTO_STYLE_NAMES: &[&str] = &["Repeat", "Meander"];
const START_MODE_NAMES: &[&str] = &["Perform", "Auto"];

const DEST_MACRO: usize = 0;
const DEST_PANIC: usize = 1;
const DEST_COUNT: usize = 2;
const TRIG_HIGH: u16 = 2458;

fn att_from_pct(pct: i32) -> u16 {
    ((pct.clamp(0, 100) as u32 * 4095) / 100) as u16
}

fn mod_u16(base: u16, in_val: u16) -> u16 {
    (base as i32 + in_val as i32 - 2047).clamp(0, 4095) as u16
}

/// Rest marker in genre rhythm DNA: `REST_CODE | bars` → silence for `bars` bars
/// (Feel-independent). `REST_CODE` alone → half-bar breath.
const REST_CODE: u8 = 0x80;

/// Hit duration for rhythm weight 1: laid back (feel=0) … advanced (feel=4095).
const FEEL_HIT_LONG: u32 = 64;
const FEEL_HIT_SHORT: u32 = 5;

/// Sentinel: no deferred note-on/off scheduled.
const NO_AT: u32 = u32::MAX;

/// Auto Feel fader → ticks per rhythm weight unit (low = laid back, high = advanced).
fn feel_hit_unit(feel: u16) -> u32 {
    let span = FEEL_HIT_LONG - FEEL_HIT_SHORT;
    FEEL_HIT_LONG - (u32::from(feel) * span) / 4095
}

fn is_rhythm_rest(weight: u8) -> bool {
    weight == 0 || weight >= REST_CODE
}

/// Genre rhythm step duration.
/// - `1..=127` (below [`REST_CODE`]): hit for `feel_unit × weight` ticks
/// - [`REST_CODE`] `| bars`: rest for `bars` bars (fixed; 0 bars → half-bar)
/// - `0`: half-bar breath (legacy)
fn segment_ticks(feel: u16, weight: u8) -> u32 {
    if weight >= REST_CODE {
        let bars = weight & 0x7f;
        if bars == 0 {
            TICKS_PER_BAR / 2
        } else {
            u32::from(bars).saturating_mul(TICKS_PER_BAR)
        }
    } else if weight == 0 {
        TICKS_PER_BAR / 2
    } else {
        feel_hit_unit(feel)
            .saturating_mul(u32::from(weight))
            .max(1)
    }
}

/// Absolute swing % from scene storage (0–100), seeded from genre bias.
fn swing_pct(stored: u8) -> i32 {
    i32::from(stored.min(100))
}

fn swing_from_genre(genre: usize) -> u8 {
    GenrePreset::get(genre).swing_bias.clamp(0, 100) as u8
}

/// Odd-16th delay in ticks (Grooves-style). `swing_pct` 0 = straight, 100 ≈ half a 16th.
fn swing_delay_ticks(grid_step: u32, swing_pct: i32) -> u32 {
    let pct = swing_pct.clamp(0, 100) as u32;
    if pct == 0 || grid_step.is_multiple_of(2) {
        return 0;
    }
    let max_delay = (CAPTURE_QUANT_TICKS.saturating_sub(1)).max(1);
    ((max_delay * pct) / 100).min(max_delay)
}

/// Scale a captured duration by Feel (laid back longer, advanced shorter).
fn groove_duration(raw_dur: u32, feel: u16) -> u32 {
    let unit = feel_hit_unit(feel).max(1);
    // Mid feel (~2048) ≈ unity; map unit range onto ~0.55x … 1.35x
    let mid = (FEEL_HIT_LONG + FEEL_HIT_SHORT) / 2;
    (raw_dur.saturating_mul(unit) / mid.max(1)).max(1)
}

/// Gate length inside a hit segment — short absolute stabs; weight only spaces
/// onsets. Rest DNA still owns multi-bar breaks.
/// Laid (~0) ≈ 18 ticks (~3/16); advanced (~4095) ≈ 4 ticks. Never >35% of segment.
fn hit_gate_ticks(segment: u32, feel: u16, swing_delay: u32) -> u32 {
    let usable = segment.saturating_sub(swing_delay).max(1);
    if usable <= 1 {
        return 1;
    }
    const STAB_LAID: u32 = 18;
    const STAB_ADV: u32 = 4;
    let stab = STAB_LAID - (u32::from(feel) * (STAB_LAID - STAB_ADV)) / 4095;
    let cap = (usable.saturating_mul(35) / 100).max(1);
    stab.min(cap).min(usable.saturating_sub(1).max(1)).max(1)
}

pub static CONFIG: Config<PARAMS> = Config::new(
    "Chord Vamp",
    "Chord progressions — perform, shift+tap capture, or auto vamp",
    Color::Violet,
    AppIcon::NoteGrid,
)
.add_param(Param::MidiOut)
.add_param(Param::MidiChannel {
    name: "MIDI Channel",
})
.add_param(Param::MidiNote {
    name: "Root",
})
.add_param(Param::Enum {
    name: "Scale",
    variants: SCALE_NAMES,
})
.add_param(Param::Enum {
    name: "Genre",
    variants: GENRE_NAMES,
})
.add_param(Param::Enum {
    name: "Voicing",
    variants: VOICING_NAMES,
})
.add_param(Param::i32 {
    name: "Velocity",
    min: 1,
    max: 127,
})
.add_param(Param::Enum {
    name: "Auto style",
    variants: AUTO_STYLE_NAMES,
})
.add_param(Param::Enum {
    name: "Start mode",
    variants: START_MODE_NAMES,
})
.add_param(Param::Enum {
    name: "Capture length",
    variants: CAPTURE_NAMES,
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
})
.add_param(Param::Enum {
    name: "Jack",
    variants: &["CV Out", "CV In"],
})
.add_param(Param::Range {
    name: "Range",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::VoltPerOct)
.add_param(Param::Enum {
    name: "CV Dest",
    variants: &["Macro", "Panic"],
})
.add_param(Param::i32 {
    name: "CV Att",
    min: 0,
    max: 100,
});

pub struct Params {
    midi_out: MidiOut,
    midi_channel: MidiChannel,
    root: MidiNote,
    scale: usize,
    genre: usize,
    voicing: usize,
    velocity: i32,
    auto_style: usize,
    start_mode: usize,
    capture_len: usize,
    color: Color,
    cv_jack: usize,
    range: Range,
    vpo: VoltPerOct,
    cv_dest: usize,
    cv_att: i32,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < 11 {
            return None;
        }
        // Legacy (≤13): always CV In. (16): jack pack.
        let (cv_jack, range, vpo, cv_dest, cv_att) = if values.len() >= 16 {
            (
                usize::from_value(values[11]).min(1),
                Range::from_value(values[12]),
                VoltPerOct::from_value(values[13]),
                usize::from_value(values[14]).min(DEST_COUNT - 1),
                i32::from_value(values[15]).clamp(0, 100),
            )
        } else if values.len() >= 13 {
            (
                CV_JACK_IN,
                Range::_Neg5_5V,
                VoltPerOct::Standard,
                usize::from_value(values[11]).min(DEST_COUNT - 1),
                i32::from_value(values[12]).clamp(0, 100),
            )
        } else {
            (CV_JACK_IN, Range::_Neg5_5V, VoltPerOct::Standard, DEST_MACRO, 100)
        };
        Some(Self {
            midi_out: MidiOut::from_value(values[0]),
            midi_channel: MidiChannel::from_value(values[1]),
            root: MidiNote::from_value(values[2]),
            scale: usize::from_value(values[3]),
            genre: usize::from_value(values[4]).min(NUM_GENRES - 1),
            voicing: usize::from_value(values[5]),
            velocity: i32::from_value(values[6]).clamp(1, 127),
            auto_style: usize::from_value(values[7]),
            start_mode: usize::from_value(values[8]),
            capture_len: usize::from_value(values[9]).min(CAPTURE_BARS.len() - 1),
            color: Color::from_value(values[10]),
            cv_jack,
            range,
            vpo,
            cv_dest,
            cv_att,
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.midi_channel.into()).unwrap();
        vec.push(self.root.into()).unwrap();
        vec.push(self.scale.into()).unwrap();
        vec.push(self.genre.into()).unwrap();
        vec.push(self.voicing.into()).unwrap();
        vec.push(self.velocity.into()).unwrap();
        vec.push(self.auto_style.into()).unwrap();
        vec.push(self.start_mode.into()).unwrap();
        vec.push(self.capture_len.into()).unwrap();
        vec.push(self.color.into()).unwrap();
        vec.push(self.cv_jack.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.vpo.into()).unwrap();
        vec.push(self.cv_dest.into()).unwrap();
        vec.push(self.cv_att.into()).unwrap();
        vec
    }
}

#[derive(Serialize, Deserialize)]
pub struct Storage {
    /// Active vamp degrees (I=0 … vii=6) for Perform scrub / genre Auto.
    slots: [u8; VAMP_CAP],
    slot_count: u8,
    genre: u8,
    scrub: u16,
    tension: u16,
    div_fader: u16,
    mode_auto: bool,
    meander: bool,
    auto_running: bool,
    /// Timed capture clip (relative on/dur within `clip_bars` bars).
    clip_active: bool,
    clip_len: u8,
    clip_bars: u8,
    clip_deg: [u8; VAMP_CAP],
    clip_on: [u16; VAMP_CAP],
    clip_dur: [u16; VAMP_CAP],
    /// Odd-16th delay 0–100; seeded from genre `swing_bias` (live latch later).
    swing: u8,
}

impl Default for Storage {
    fn default() -> Self {
        let mut slots = [0u8; VAMP_CAP];
        let n = load_classic_into(&mut slots, 3); // House
        Self {
            slots,
            slot_count: n,
            genre: 3,
            scrub: 0,
            tension: 2048,
            div_fader: 1536, // mid Feel (laid back ↔ advanced)
            mode_auto: false,
            meander: false,
            auto_running: true,
            clip_active: false,
            clip_len: 0,
            clip_bars: 8,
            clip_deg: [0u8; VAMP_CAP],
            clip_on: [0u16; VAMP_CAP],
            clip_dur: [0u16; VAMP_CAP],
            swing: swing_from_genre(3),
        }
    }
}

impl AppStorage for Storage {}

#[derive(Clone, Copy)]
struct GenrePreset {
    /// Classic trope loop — used as default / occasional seed (~20%).
    progression: &'static [u8],
    /// Markov weights from → to (rows sum arbitrary; sampled by weight).
    markov: &'static [[u8; NUM_DEGREES]; NUM_DEGREES],
    /// Auto vamp rhythm phrase (loops; does not meander — only harmony does).
    /// `1..=n` = chord weight (× Feel); [`REST_CODE`]`|bars` = silence for `bars`
    /// bars (Feel-independent). Play a bar or two, then a long break.
    rhythm: &'static [u8],
    /// Genre default swing % (0–100); copied into Storage on genre pick/reseed.
    swing_bias: i8,
}

impl GenrePreset {
    fn get(index: usize) -> Self {
        GENRES[index.min(NUM_GENRES - 1)]
    }
}

/// Genre chord + rhythm DNA (degrees 0–6). Markov biases genre-typical moves.
/// Rhythm: a multi-chord statement (~1–2 bars at mid Feel), then a long break —
/// not one stab and silence.
const GENRES: [GenrePreset; NUM_GENRES] = [
    // Dub — i–IV–i–V; half-time 4-chord statement, long space
    GenrePreset {
        progression: &[0, 3, 0, 4, 0, 3, 0, 4],
        markov: &[
            [4, 1, 1, 6, 5, 2, 2],
            [3, 2, 1, 2, 3, 2, 1],
            [2, 2, 2, 2, 2, 3, 2],
            [5, 1, 1, 2, 4, 2, 1],
            [6, 1, 1, 3, 2, 2, 2],
            [3, 2, 2, 2, 2, 2, 3],
            [4, 1, 1, 2, 3, 2, 2],
        ],
        rhythm: &[2, 1, 2, 1, REST_CODE | 4, 2, 1, 2, REST_CODE | 3],
        swing_bias: 20,
    },
    // Disco — I–vi–IV–V; busy 2-bar vamp, shorter breaks
    GenrePreset {
        progression: &[0, 5, 3, 4, 0, 5, 3, 4],
        markov: &[
            [2, 1, 1, 4, 5, 6, 1],
            [2, 2, 2, 2, 3, 2, 2],
            [2, 2, 2, 3, 2, 3, 2],
            [3, 1, 1, 2, 6, 2, 1],
            [6, 1, 1, 3, 2, 2, 1],
            [3, 1, 2, 5, 3, 2, 1],
            [4, 1, 1, 2, 4, 2, 1],
        ],
        rhythm: &[1, 1, 1, 1, 1, 1, 2, REST_CODE | 2, 1, 1, 1, 1, REST_CODE | 3],
        swing_bias: 35,
    },
    // Hip-Hop — i–VI–III–VII; boom-bap phrase then wide open
    GenrePreset {
        progression: &[0, 5, 2, 6, 0, 5, 2, 6],
        markov: &[
            [3, 1, 4, 2, 2, 5, 4],
            [2, 2, 2, 2, 2, 3, 2],
            [3, 1, 2, 2, 2, 4, 3],
            [3, 2, 2, 2, 3, 2, 2],
            [4, 1, 2, 2, 2, 3, 3],
            [4, 1, 3, 2, 2, 2, 3],
            [5, 1, 2, 2, 2, 3, 2],
        ],
        rhythm: &[2, 1, 1, 2, REST_CODE | 3, 2, 1, 1, REST_CODE | 5],
        swing_bias: 40,
    },
    // House — i–VII–VI–VII; pump through the loop, uneven breaks
    GenrePreset {
        progression: &[0, 6, 5, 6, 0, 6, 5, 6],
        markov: &[
            [3, 1, 1, 2, 2, 4, 6],
            [2, 2, 2, 2, 2, 3, 2],
            [2, 2, 2, 2, 2, 3, 3],
            [3, 1, 1, 2, 3, 3, 3],
            [3, 1, 1, 2, 2, 3, 4],
            [4, 1, 1, 2, 2, 2, 5],
            [5, 1, 1, 2, 2, 4, 2],
        ],
        rhythm: &[1, 1, 1, 1, 2, REST_CODE | 2, 1, 1, 1, 2, REST_CODE | 4],
        swing_bias: 30,
    },
    // Techno — static/minimal; long holds across the statement, rare deep drop
    GenrePreset {
        progression: &[0, 0, 0, 4, 0, 0, 0, 4],
        markov: &[
            [8, 1, 1, 2, 4, 2, 3],
            [4, 2, 1, 1, 2, 1, 1],
            [3, 1, 2, 1, 2, 1, 2],
            [4, 1, 1, 2, 3, 1, 2],
            [6, 1, 1, 2, 2, 2, 2],
            [4, 1, 1, 2, 2, 2, 3],
            [5, 1, 1, 1, 3, 2, 2],
        ],
        rhythm: &[3, 2, 3, REST_CODE | 1, 2, 3, REST_CODE | 5],
        swing_bias: 8,
    },
    // Trip-Hop — i–VII–VI–v; sparse but still a short progression, then void
    GenrePreset {
        progression: &[0, 6, 5, 4, 0, 6, 5, 4],
        markov: &[
            [3, 1, 2, 2, 4, 4, 5],
            [2, 2, 2, 2, 2, 3, 2],
            [2, 2, 2, 2, 3, 3, 2],
            [3, 1, 2, 2, 3, 3, 2],
            [4, 1, 1, 2, 2, 3, 3],
            [4, 1, 2, 2, 3, 2, 4],
            [5, 1, 1, 2, 3, 4, 2],
        ],
        rhythm: &[2, 1, 2, REST_CODE | 4, 1, 2, 1, REST_CODE | 5],
        swing_bias: 45,
    },
    // UK Garage — i–III–VI–VII; choppy 2-bar then break
    GenrePreset {
        progression: &[0, 2, 5, 6, 0, 2, 5, 6],
        markov: &[
            [3, 1, 5, 2, 2, 4, 4],
            [2, 2, 3, 2, 2, 3, 2],
            [3, 2, 2, 2, 2, 4, 3],
            [3, 1, 2, 2, 3, 2, 2],
            [3, 1, 2, 2, 2, 3, 3],
            [4, 1, 3, 2, 2, 2, 4],
            [5, 1, 2, 2, 2, 3, 2],
        ],
        rhythm: &[1, 1, 2, 1, 1, REST_CODE | 2, 2, 1, 1, 1, REST_CODE | 3],
        swing_bias: 50,
    },
    // Dubstep — i–i–VI–VII; half-time progression, wide gaps
    GenrePreset {
        progression: &[0, 0, 5, 6, 0, 0, 5, 6],
        markov: &[
            [6, 1, 1, 2, 2, 5, 5],
            [3, 2, 1, 1, 2, 2, 2],
            [3, 1, 2, 1, 2, 2, 2],
            [3, 1, 1, 2, 3, 2, 2],
            [4, 1, 1, 2, 2, 3, 3],
            [4, 1, 1, 2, 2, 2, 5],
            [5, 1, 1, 1, 3, 4, 2],
        ],
        rhythm: &[2, 2, 1, 2, REST_CODE | 3, 2, 1, 2, REST_CODE | 5],
        swing_bias: 25,
    },
];

fn midi_u8(note: MidiNote) -> u8 {
    u7::from(note).as_int()
}

fn note_to_pitch(note: u8) -> Pitch {
    // MIDI note 0 = C-1 → octave -1; MIDI 60 = C4 → octave 4.
    let note = note.min(127);
    let octave = (note as i16 / 12) - 1;
    Pitch {
        octave: octave as i8,
        note: Note::from(note % 12),
        raw: None,
    }
}

fn scale_index_to_key(index: usize) -> Key {
    match index {
        1 => Key::Dorian,
        2 => Key::Phrygian,
        3 => Key::Lydian,
        4 => Key::Mixolydian,
        5 => Key::Aeolian,
        6 => Key::Locrian,
        7 => Key::PentatonicMaj,
        8 => Key::PentatonicMin,
        _ => Key::Ionian,
    }
}

fn scale_offsets(key: Key) -> Vec<u8, 12> {
    let mask = key.as_u16_key();
    let mut notes = Vec::new();
    for i in 0..12u8 {
        if (mask >> (11 - i)) & 1 != 0 {
            let _ = notes.push(i);
        }
    }
    if notes.is_empty() {
        let _ = notes.push(0);
    }
    notes
}

fn velocity_12bit(vel_7: i32) -> u16 {
    let v = vel_7.clamp(1, 127) as u32;
    ((v * 4095) / 127) as u16
}

/// Build MIDI note numbers for a diatonic chord on `degree` (0=I … 6=vii).
/// `octave` transposes the whole voicing up by N octaves from `root_midi`.
fn build_chord(
    root_midi: u8,
    key: Key,
    degree: u8,
    voicing: usize,
    octave: u8,
) -> Vec<u8, SOUNDING_CAP> {
    let scale = scale_offsets(key);
    let n = scale.len();
    let ext: &[usize] = match voicing {
        1 => &[0, 2, 4, 6],
        2 => &[0, 2, 4, 6, 8],
        _ => &[0, 2, 4],
    };

    let mut out: Vec<u8, SOUNDING_CAP> = Vec::new();
    let base_degree = degree as usize % NUM_DEGREES;
    let oct_shift = 12u16 * u16::from(octave);

    for (vi, &steps) in ext.iter().enumerate() {
        let d = base_degree + steps;
        let oct = (d / n) as i16;
        let idx = d % n;
        let semis = oct * 12 + scale[idx] as i16;
        // Degree 0 / steps 0 → scale[0] which is 0 relative to tonic → root_midi
        let mut note = (root_midi as i16 + semis - scale[0] as i16).clamp(0, 127) as u8;
        note = (u16::from(note).saturating_add(oct_shift)).min(127) as u8;
        if voicing == 3 && vi > 0 {
            note = (note as u16).saturating_add(12u16 * vi as u16).min(127) as u8;
        }
        let _ = out.push(note);
    }
    out
}

/// Unique degrees present in `slots[..count]`, sorted low→high (I … vii).
fn unique_degrees(slots: &[u8; VAMP_CAP], count: usize) -> Vec<u8, NUM_DEGREES> {
    let mut seen = [false; NUM_DEGREES];
    let n = count.min(VAMP_CAP);
    for &d in slots.iter().take(n) {
        seen[(d as usize) % NUM_DEGREES] = true;
    }
    let mut out = Vec::new();
    for (deg, present) in seen.iter().enumerate() {
        if *present {
            let _ = out.push(deg as u8);
        }
    }
    if out.is_empty() {
        let _ = out.push(0);
    }
    out
}

/// Perform palette: genre degrees ascending, repeated across [`PERFORM_OCTAVES`].
/// Returns number of entries written to `deg_out` / `oct_out`.
fn build_perform_map(
    slots: &[u8; VAMP_CAP],
    count: usize,
    deg_out: &mut [u8; PERFORM_CAP],
    oct_out: &mut [u8; PERFORM_CAP],
) -> u8 {
    let degrees = unique_degrees(slots, count);
    let mut n = 0usize;
    for oct in 0..PERFORM_OCTAVES {
        for &deg in degrees.iter() {
            if n >= PERFORM_CAP {
                break;
            }
            deg_out[n] = deg;
            oct_out[n] = oct as u8;
            n += 1;
        }
    }
    for i in n..PERFORM_CAP {
        deg_out[i] = 0;
        oct_out[i] = 0;
    }
    n.max(1) as u8
}

fn apply_perform_index(
    idx: usize,
    deg_map: &[u8; PERFORM_CAP],
    oct_map: &[u8; PERFORM_CAP],
    count: u8,
) -> (u8, u8) {
    let i = idx.min(count.max(1) as usize - 1).min(PERFORM_CAP - 1);
    (deg_map[i], oct_map[i])
}

fn pack_deg_oct(degree: u8, octave: u8) -> u8 {
    (degree & 0x0f) | (octave.saturating_mul(16))
}

fn unpack_deg_oct(packed: u8) -> (u8, u8) {
    (packed & 0x0f, packed >> 4)
}

fn pick_markov(from: u8, weights: &[[u8; NUM_DEGREES]; NUM_DEGREES], die: &Die) -> u8 {
    let row = &weights[(from as usize) % NUM_DEGREES];
    pick_weighted(row, die).unwrap_or(from)
}

fn pick_weighted(weights: &[u8; NUM_DEGREES], die: &Die) -> Option<u8> {
    let total: u32 = weights.iter().map(|&w| w as u32).sum();
    if total == 0 {
        return None;
    }
    let mut pick = (die.roll() as u32 * total) / 4096;
    for (i, &w) in weights.iter().enumerate() {
        if pick < w as u32 {
            return Some(i as u8);
        }
        pick = pick.saturating_sub(w as u32);
    }
    Some((NUM_DEGREES - 1) as u8)
}

/// Deterministic classic genre loop (boot / empty storage).
/// Tiles the short genre trope out to [`NUM_SLOTS`].
fn load_classic_into(slots: &mut [u8; VAMP_CAP], genre: usize) -> u8 {
    let g = GenrePreset::get(genre);
    let src = g.progression;
    let n = NUM_SLOTS.min(VAMP_CAP);
    if src.is_empty() {
        for s in slots.iter_mut().take(n) {
            *s = 0;
        }
    } else {
        for (i, slot) in slots.iter_mut().enumerate().take(n) {
            *slot = src[i % src.len()];
        }
    }
    for s in slots.iter_mut().skip(n) {
        *s = 0;
    }
    n as u8
}

/// Genre-biased progression seed: mostly Markov walk, sometimes the classic trope.
/// Shift+Fader and Auto+Long use this so each pick/reroll feels fresh but on-style.
fn seed_genre_into(slots: &mut [u8; VAMP_CAP], genre: usize, die: &Die) -> u8 {
    let g = GenrePreset::get(genre);
    let n = NUM_SLOTS.min(VAMP_CAP);

    // ~20%: recognizable canned loop for that genre.
    if die.roll() < 820 {
        return load_classic_into(slots, genre);
    }

    // Start on tonic often; otherwise sample from tonic's Markov row (genre prior).
    let mut deg = if die.roll() < 2700 {
        0
    } else {
        pick_weighted(&g.markov[0], die).unwrap_or(0)
    };

    for (i, slot) in slots.iter_mut().enumerate().take(n) {
        *slot = deg;
        // Soft phrase reset toward tonic every 4 slots.
        if (i + 1) % 4 == 0 && die.roll() < 1600 {
            deg = 0;
        } else {
            deg = pick_markov(deg, g.markov, die);
        }
    }
    for s in slots.iter_mut().skip(n) {
        *s = 0;
    }
    // Prefer a dominant/leading tone into the loop-back to i.
    if die.roll() < 2200 {
        slots[n - 1] = if die.roll() < 2048 { 4 } else { 6 };
    }
    n as u8
}

/// Snap a tick to the nearest capture grid (used for note-on only).
fn quantize_tick_nearest(t: u32, grid: u32) -> u32 {
    if grid <= 1 {
        return t;
    }
    let q = t / grid;
    let rem = t % grid;
    if rem * 2 >= grid {
        q.saturating_add(1).saturating_mul(grid)
    } else {
        q.saturating_mul(grid)
    }
}

/// Build a relative timed clip from the always-record ring (last `bars` bars).
/// Starts are quantized to [`CAPTURE_QUANT_TICKS`]; durations keep the played length.
/// Empty lead-in is cropped so the first chord lands on loop phase 0 (downbeat playhead).
#[allow(clippy::too_many_arguments)]
fn build_clip_from_ring(
    bars: u32,
    now: u32,
    ring_len: usize,
    ring_write: usize,
    deg: &[u8; RING_CAP],
    ons: &[u32; RING_CAP],
    offs: &[u32; RING_CAP],
    open_idx: u8,
) -> (u8, [u8; VAMP_CAP], [u16; VAMP_CAP], [u16; VAMP_CAP]) {
    let window = bars.saturating_mul(TICKS_PER_BAR);
    let clip_start = now.wrapping_sub(window);
    let mut out_deg = [0u8; VAMP_CAP];
    let mut out_on = [0u16; VAMP_CAP];
    let mut out_dur = [0u16; VAMP_CAP];
    let mut n = 0usize;

    if ring_len == 0 || window == 0 {
        return (0, out_deg, out_on, out_dur);
    }

    for i in 0..ring_len {
        let idx = if ring_len < RING_CAP {
            i
        } else {
            (ring_write + i) % RING_CAP
        };
        let on = ons[idx];
        let age = now.wrapping_sub(on);
        if age >= window || age >= (u32::MAX / 2) {
            continue;
        }
        let mut off = offs[idx];
        if off == 0 || (open_idx as usize == idx) {
            off = now;
        }
        // Actual hold length (not quantized).
        let dur_abs = off.wrapping_sub(on);
        if dur_abs == 0 || dur_abs >= (u32::MAX / 2) {
            continue;
        }
        let rel_raw = on.wrapping_sub(clip_start);
        if rel_raw >= window {
            continue;
        }
        // Quantize start only; keep played duration (clamp after lead-in crop).
        let mut rel_on = quantize_tick_nearest(rel_raw, CAPTURE_QUANT_TICKS);
        if rel_on >= window {
            // Nearest grid landed on/past the loop point — pull back one step.
            rel_on = window.saturating_sub(CAPTURE_QUANT_TICKS.min(window));
            rel_on = (rel_on / CAPTURE_QUANT_TICKS.max(1)) * CAPTURE_QUANT_TICKS.max(1);
        }
        if n >= VAMP_CAP {
            continue;
        }
        out_deg[n] = deg[idx];
        out_on[n] = rel_on as u16;
        out_dur[n] = dur_abs.max(1).min(u16::MAX as u32) as u16;
        n += 1;
    }

    // Crop empty lead-in: first signal → phase 0 when playback hits the downbeat.
    if n > 0 {
        let mut min_on = u16::MAX;
        for on in out_on.iter().take(n) {
            min_on = min_on.min(*on);
        }
        if min_on > 0 {
            for on in out_on.iter_mut().take(n) {
                *on = on.saturating_sub(min_on);
            }
        }
        for i in 0..n {
            let on = out_on[i] as u32;
            let mut dur = out_dur[i] as u32;
            if on.saturating_add(dur) > window {
                dur = window.saturating_sub(on);
            }
            if dur == 0 {
                dur = 1;
            }
            out_dur[i] = dur as u16;
        }
    }

    (n as u8, out_deg, out_on, out_dur)
}

#[embassy_executor::task(pool_size = 16 / CHANNELS)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(
        app.app_id,
        app.layout_id,
        Params {
            midi_out: MidiOut::default(),
            midi_channel: MidiChannel::default(),
            root: MidiNote::from(48),
            scale: 5, // Aeolian
            genre: 3, // House
            voicing: 0,
            velocity: 100,
            auto_style: 0,
            start_mode: 0,
            capture_len: 1, // 8 bars
            color: Color::Violet,
            cv_jack: CV_JACK_IN,
            range: Range::_0_10V,
            vpo: VoltPerOct::Standard,
            cv_dest: DEST_MACRO,
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
    let (
        midi_out_cfg,
        midi_chan,
        root,
        scale_idx,
        genre_param,
        voicing,
        velocity,
        auto_style,
        start_mode,
        capture_len,
        led_color,
        cv_jack,
        range,
        vpo,
        cv_dest,
        cv_att,
    ) = params.query(|p| {
        (
            p.midi_out,
            p.midi_channel,
            p.root,
            p.scale,
            p.genre.min(NUM_GENRES - 1),
            p.voicing,
            p.velocity,
            p.auto_style,
            p.start_mode,
            p.capture_len.min(CAPTURE_BARS.len() - 1),
            p.color,
            p.cv_jack.min(1),
            p.range,
            p.vpo,
            p.cv_dest.min(DEST_COUNT - 1),
            att_from_pct(p.cv_att),
        )
    });

    let root_midi = midi_u8(root);
    let key = scale_index_to_key(scale_idx);
    let vel12 = velocity_12bit(velocity);

    let fader = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
    let mut clock = app.use_clock();
    let ticks = clock.get_ticker();
    let die = app.use_die();
    let midi = app.use_midi_output(midi_out_cfg, midi_chan, false);
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
        jack.set_value(0);
    }

    let glob_latch = app.make_global(LatchLayer::Main);
    let glob_mode_auto = app.make_global(false);
    let glob_meander = app.make_global(false);
    let glob_auto_running = app.make_global(false);
    let glob_scrub = app.make_global(0u16);
    let glob_tension = app.make_global(2048u16);
    let glob_feel = app.make_global(1536u16);
    let glob_swing = app.make_global(0u8);
    let glob_genre = app.make_global(genre_param);
    let glob_slot_count = app.make_global(NUM_SLOTS as u8);
    let glob_slot_idx = app.make_global(0u8);
    let glob_degree = app.make_global(0u8);
    let glob_octave = app.make_global(0u8);
    let glob_perform_count = app.make_global(PERFORM_CAP as u8);
    let glob_perform_idx = app.make_global(0u8);
    let glob_activity = app.make_global(0u8);
    let glob_capture_flash = app.make_global(0u8);
    let long_press_fired = app.make_global(false);
    let panic_flag = app.make_global(false);
    let glob_cv_val = app.make_global(2047u16);
    let chord_held = app.make_global(false);
    let ring_write = app.make_global(0u8);
    let ring_len = app.make_global(0u8);
    let ring_open = app.make_global(255u8); // 255 = no open note-on
    // Clock watch → voice engine (never await MIDI inside the clock subscriber).
    let pending_play = app.make_global(false);
    let pending_degree = app.make_global(0u8);
    let pending_octave = app.make_global(0u8);
    let pending_release = app.make_global(false);
    let pending_record = app.make_global(false);
    let pending_on_at = app.make_global(NO_AT);
    let pending_off_at = app.make_global(NO_AT);

    // Shared vamp slots + timed ring + capture clip.
    let slots_cell = app.make_global([0u8; VAMP_CAP]);
    let perform_deg_cell = app.make_global([0u8; PERFORM_CAP]);
    let perform_oct_cell = app.make_global([0u8; PERFORM_CAP]);
    let ring_deg = app.make_global([0u8; RING_CAP]);
    let ring_on = app.make_global([0u32; RING_CAP]);
    let ring_off = app.make_global([0u32; RING_CAP]);
    let clip_deg_cell = app.make_global([0u8; VAMP_CAP]);
    let clip_on_cell = app.make_global([0u16; VAMP_CAP]);
    let clip_dur_cell = app.make_global([0u16; VAMP_CAP]);
    let glob_clip_active = app.make_global(false);
    let glob_clip_armed = app.make_global(false);
    let glob_clip_origin = app.make_global(0u32);
    let glob_clip_len = app.make_global(0u8);
    let glob_clip_bars = app.make_global(8u8);
    let glob_last_genre = app.make_global(genre_param);
    let glob_loop_flash = app.make_global(0u8);
    // Continuous Alt-layer fader value (not reconstructed from genre index — that
    // packed all genres into a short span at the top when sweeping down).
    let glob_genre_fader = app.make_global(0u16);

    let (
        st_slots,
        st_count,
        st_genre,
        st_scrub,
        st_tension,
        st_div,
        st_auto,
        st_meander,
        st_running,
        st_clip_active,
        st_clip_len,
        st_clip_bars,
        st_clip_deg,
        st_clip_on,
        st_clip_dur,
        st_swing,
    ) = storage.query(|s| {
        (
            s.slots,
            s.slot_count,
            s.genre as usize,
            s.scrub,
            s.tension,
            s.div_fader,
            s.mode_auto,
            s.meander,
            s.auto_running,
            s.clip_active,
            s.clip_len,
            s.clip_bars,
            s.clip_deg,
            s.clip_on,
            s.clip_dur,
            s.swing,
        )
    });

    let mut slots = st_slots;
    let mut slot_count = st_count;
    if slot_count == 0 {
        slot_count = load_classic_into(&mut slots, genre_param);
    } else if slot_count as usize > NUM_SLOTS {
        // Migrate longer pre-split progressions down to the auto vamp length.
        slot_count = NUM_SLOTS as u8;
    }
    let genre = if st_genre < NUM_GENRES {
        st_genre
    } else {
        genre_param
    };
    // Live mode lives in storage (Shift+Button). Start mode applied below on pristine storage.
    let mut mode_auto = st_auto;
    if !st_auto
        && !st_running
        && st_scrub == 0
        && st_tension == 2048
        && st_div == 1536
        && start_mode == 1
    {
        mode_auto = true;
    }
    let meander = st_meander || auto_style == 1;

    slots_cell.set(slots);
    glob_slot_count.set(slot_count);
    let use_clip_init = st_clip_active && st_clip_len > 0;
    let genre_init = if use_clip_init { CAPTURE_SLOT } else { genre };
    let picks_init = if use_clip_init {
        NUM_GENRES + 1
    } else {
        NUM_GENRES
    };
    glob_genre.set(genre_init);
    glob_genre_fader.set(genre_fader_center(genre_init, picks_init));
    glob_last_genre.set(genre.min(NUM_GENRES - 1));
    // Heal ParamStore if hardware/storage genre drifted from the last configurator write.
    if genre < NUM_GENRES && genre != genre_param {
        params.update(|p| p.genre = genre).await;
    }
    glob_scrub.set(st_scrub);
    glob_tension.set(st_tension);
    glob_feel.set(st_div);
    glob_swing.set(st_swing.min(100));
    glob_mode_auto.set(mode_auto);
    glob_meander.set(meander);
    // Remember play/pause across Perform↔Auto; only gated by mode_auto when ticking.
    glob_auto_running.set(st_running);
    clip_deg_cell.set(st_clip_deg);
    clip_on_cell.set(st_clip_on);
    clip_dur_cell.set(st_clip_dur);
    glob_clip_len.set(st_clip_len);
    glob_clip_bars.set(st_clip_bars.max(1));
    glob_clip_active.set(st_clip_active && st_clip_len > 0);
    {
        let mut pdeg = [0u8; PERFORM_CAP];
        let mut poct = [0u8; PERFORM_CAP];
        let pc = build_perform_map(&slots, slot_count as usize, &mut pdeg, &mut poct);
        perform_deg_cell.set(pdeg);
        perform_oct_cell.set(poct);
        glob_perform_count.set(pc);
        let idx = value_to_index(st_scrub, pc.max(1) as usize);
        let (deg, oct) = apply_perform_index(idx, &pdeg, &poct, pc);
        glob_perform_idx.set(idx as u8);
        glob_degree.set(deg);
        glob_octave.set(oct);
        // Auto slot cursor starts at scrub mapped into progression length.
        let auto_idx = value_to_index(st_scrub, slot_count.max(1) as usize);
        glob_slot_idx.set(auto_idx as u8);
    }

    leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);

    let refresh_perform_map = |slots: &[u8; VAMP_CAP], count: u8| {
        let mut pdeg = [0u8; PERFORM_CAP];
        let mut poct = [0u8; PERFORM_CAP];
        let pc = build_perform_map(slots, count as usize, &mut pdeg, &mut poct);
        perform_deg_cell.set(pdeg);
        perform_oct_cell.set(poct);
        glob_perform_count.set(pc);
        let idx = value_to_index(glob_scrub.get(), pc.max(1) as usize);
        let (deg, oct) = apply_perform_index(idx, &pdeg, &poct, pc);
        glob_perform_idx.set(idx as u8);
        if !glob_mode_auto.get() {
            glob_degree.set(deg);
            glob_octave.set(oct);
        }
    };

    let push_ring_on = |degree: u8, octave: u8, on: u32| {
        // Close any dangling open event at this tick.
        let open = ring_open.get() as usize;
        if open < RING_CAP {
            let mut offs = ring_off.get();
            if offs[open] == 0 {
                offs[open] = on.max(1);
                ring_off.set(offs);
            }
        }
        let mut deg = ring_deg.get();
        let mut ons = ring_on.get();
        let mut offs = ring_off.get();
        let w = ring_write.get() as usize;
        deg[w] = pack_deg_oct(degree, octave);
        ons[w] = on;
        offs[w] = 0; // open
        ring_deg.set(deg);
        ring_on.set(ons);
        ring_off.set(offs);
        ring_open.set(w as u8);
        ring_write.set(((w + 1) % RING_CAP) as u8);
        let len = ring_len.get();
        if (len as usize) < RING_CAP {
            ring_len.set(len + 1);
        }
    };

    let close_ring = |off: u32| {
        let open = ring_open.get() as usize;
        if open >= RING_CAP {
            return;
        }
        let mut offs = ring_off.get();
        let ons = ring_on.get();
        let t = off.max(ons[open].wrapping_add(1));
        offs[open] = t;
        ring_off.set(offs);
        ring_open.set(255);
    };

    let clock_watch = async {
        let mut phrase_i: usize = 0;
        let mut harm_i: usize = 0;
        let mut segment_end: u32 = 0;
        let mut current_degree: u8 = glob_degree.get();
        loop {
            match clock.wait_for_event(ClockDivision::_1).await {
                ClockEvent::Tick => {
                    if !(glob_mode_auto.get() && glob_auto_running.get()) {
                        continue;
                    }
                    let clkn = ticks() as u32;

                    // Deferred Auto note-on / gate-off (swing start + Feel length).
                    let on_at = pending_on_at.get();
                    if on_at != NO_AT && clkn >= on_at {
                        pending_play.set(true);
                        pending_on_at.set(NO_AT);
                    }
                    let off_at = pending_off_at.get();
                    if off_at != NO_AT && clkn >= off_at {
                        pending_release.set(true);
                        pending_off_at.set(NO_AT);
                    }

                    // Timed capture clip playback
                    if glob_clip_active.get() && glob_clip_len.get() > 0 {
                        if glob_clip_armed.get() {
                            if clkn.is_multiple_of(TICKS_PER_BAR) {
                                glob_clip_origin.set(clkn);
                                glob_clip_armed.set(false);
                                glob_loop_flash.set(40);
                            }
                            continue;
                        }
                        let bars = glob_clip_bars.get().max(1) as u32;
                        let loop_len = bars.saturating_mul(TICKS_PER_BAR).max(1);
                        let phase = clkn.wrapping_sub(glob_clip_origin.get()) % loop_len;
                        if phase == 0 {
                            glob_loop_flash.set(40);
                        }
                        let n = glob_clip_len.get() as usize;
                        let degs = clip_deg_cell.get();
                        let ons = clip_on_cell.get();
                        let durs = clip_dur_cell.get();
                        let swing = swing_pct(glob_swing.get());
                        let feel = glob_feel.get();
                        for i in 0..n {
                            let raw_on = ons[i] as u32;
                            let step = raw_on / CAPTURE_QUANT_TICKS;
                            let delay = swing_delay_ticks(step, swing);
                            let on_ph = (raw_on + delay) % loop_len;
                            let dur = groove_duration(durs[i] as u32, feel);
                            let end = on_ph.saturating_add(dur);
                            let off_ph = if end >= loop_len {
                                loop_len.saturating_sub(1)
                            } else {
                                end
                            };
                            if phase == on_ph {
                                let (deg, oct) = unpack_deg_oct(degs[i]);
                                current_degree = deg;
                                glob_degree.set(deg);
                                glob_octave.set(oct);
                                glob_slot_idx.set(i as u8);
                                pending_degree.set(deg);
                                pending_octave.set(oct);
                                pending_record.set(false);
                                pending_play.set(true);
                            }
                            if off_ph == phase && dur > 0 {
                                pending_release.set(true);
                            }
                        }
                        continue;
                    }

                    // Genre vamp: rhythm phrase loops; harmony advances only on hits
                    // (Repeat = slots, Meander = Markov). Long rests do not burn chords.
                    if clkn >= segment_end {
                        pending_release.set(true);
                        pending_on_at.set(NO_AT);
                        pending_off_at.set(NO_AT);

                        let slots_now = slots_cell.get();
                        let count = glob_slot_count.get().max(1) as usize;
                        let g = glob_last_genre.get().min(NUM_GENRES - 1);
                        let genre = GenrePreset::get(g);
                        let weight = genre.rhythm[phrase_i % genre.rhythm.len()];
                        let feel = glob_feel.get();
                        let swing = swing_pct(glob_swing.get());
                        let total = segment_ticks(feel, weight);

                        if !is_rhythm_rest(weight) {
                            let degree = if glob_meander.get() {
                                let tension = if cv_dest == DEST_MACRO {
                                    mod_u16(glob_tension.get(), glob_cv_val.get())
                                } else {
                                    glob_tension.get()
                                };
                                if tension > 512 {
                                    let next = pick_markov(current_degree, genre.markov, &die);
                                    if tension < 3000 && die.roll() > tension {
                                        slots_now[harm_i % count]
                                    } else {
                                        next
                                    }
                                } else {
                                    slots_now[harm_i % count]
                                }
                            } else {
                                slots_now[harm_i % count]
                            };

                            current_degree = degree;
                            glob_degree.set(degree);
                            glob_octave.set(0);
                            glob_slot_idx.set((harm_i % count) as u8);
                            pending_degree.set(degree);
                            pending_octave.set(0);
                            pending_record.set(false);

                            let grid_step = (clkn / CAPTURE_QUANT_TICKS) % 16;
                            let delay = swing_delay_ticks(grid_step, swing).min(total.saturating_sub(1));
                            let gate = hit_gate_ticks(total, feel, delay);
                            pending_on_at.set(clkn.wrapping_add(delay));
                            pending_off_at.set(clkn.wrapping_add(delay).wrapping_add(gate));
                            harm_i = harm_i.wrapping_add(1);
                        }

                        segment_end = clkn.wrapping_add(total.max(1));
                        phrase_i = phrase_i.wrapping_add(1);
                    }
                }
                ClockEvent::Stop | ClockEvent::Reset => {
                    pending_release.set(true);
                    pending_on_at.set(NO_AT);
                    pending_off_at.set(NO_AT);
                    phrase_i = 0;
                    harm_i = 0;
                    segment_end = 0;
                    if glob_clip_active.get() {
                        glob_clip_armed.set(true);
                    }
                }
                _ => {}
            }
        }
    };

    let engine = async {
        let mut sounding: Vec<u8, SOUNDING_CAP> = Vec::new();
        let mut current_degree: u8 = glob_degree.get();
        let mut current_octave: u8 = glob_octave.get();

        let release_all = async |sounding: &mut Vec<u8, SOUNDING_CAP>| {
            for n in sounding.iter() {
                midi.send_note_off(MidiNote::from(*n)).await;
            }
            sounding.clear();
        };

        let play_degree =
            async |sounding: &mut Vec<u8, SOUNDING_CAP>, degree: u8, octave: u8, record: bool| {
                for n in sounding.iter() {
                    midi.send_note_off(MidiNote::from(*n)).await;
                }
                sounding.clear();
                let notes = build_chord(root_midi, key, degree, voicing, octave);
                for n in notes.iter() {
                    midi.send_note_on(MidiNote::from(*n), vel12).await;
                    let _ = sounding.push(*n);
                }
                if record {
                    push_ring_on(degree, octave, ticks() as u32);
                }
                glob_degree.set(degree);
                glob_octave.set(octave);
                glob_activity.set(255);
            };

        loop {
            if panic_flag.get() {
                release_all(&mut sounding).await;
                const ALL_SOUND_OFF: u8 = 120;
                const ALL_NOTES_OFF: u8 = 123;
                midi.send_cc(MidiCc::from(ALL_SOUND_OFF), 0).await;
                midi.send_cc(MidiCc::from(ALL_NOTES_OFF), 0).await;
                sounding.clear();
                chord_held.set(false);
                pending_play.set(false);
                pending_release.set(false);
                pending_on_at.set(NO_AT);
                pending_off_at.set(NO_AT);
                panic_flag.set(false);
                glob_activity.set(0);
            }

            if pending_release.get() {
                pending_release.set(false);
                release_all(&mut sounding).await;
            }

            if pending_play.get() {
                pending_play.set(false);
                let degree = pending_degree.get();
                let octave = pending_octave.get();
                let record = pending_record.get();
                pending_record.set(false);
                current_degree = degree;
                current_octave = octave;
                play_degree(&mut sounding, degree, octave, record).await;
            }

            // Perform: chord hold (Auto voice comes only from pending_play).
            if !glob_mode_auto.get() {
                if chord_held.get() {
                    let deg = glob_degree.get();
                    let oct = glob_octave.get();
                    if sounding.is_empty()
                        || current_degree != deg
                        || current_octave != oct
                    {
                        current_degree = deg;
                        current_octave = oct;
                        play_degree(&mut sounding, deg, oct, true).await;
                    }
                } else if !sounding.is_empty() {
                    close_ring(ticks() as u32);
                    release_all(&mut sounding).await;
                }
            }

            if glob_activity.get() > 0 {
                glob_activity.set(glob_activity.get().saturating_sub(8));
            }
            if glob_capture_flash.get() > 0 {
                glob_capture_flash.set(glob_capture_flash.get().saturating_sub(4));
            }
            if glob_loop_flash.get() > 0 {
                glob_loop_flash.set(glob_loop_flash.get().saturating_sub(2));
            }

            app.delay_millis(1).await;
        }
    };

    let button_handler = async {
        // Commit the always-record ring into a timed clip and jump to Auto+Play.
        let commit_capture = || -> bool {
            close_ring(ticks() as u32);
            chord_held.set(false);
            pending_release.set(true);

            let bars = CAPTURE_BARS[capture_len];
            let now = ticks() as u32;
            let (n, degs, ons, durs) = build_clip_from_ring(
                bars,
                now,
                ring_len.get() as usize,
                ring_write.get() as usize,
                &ring_deg.get(),
                &ring_on.get(),
                &ring_off.get(),
                ring_open.get(),
            );

            if n == 0 {
                // Bright enough to notice: empty ring / clock not advancing.
                glob_capture_flash.set(200);
                return false;
            }

            clip_deg_cell.set(degs);
            clip_on_cell.set(ons);
            clip_dur_cell.set(durs);
            glob_clip_len.set(n);
            glob_clip_bars.set(bars.min(255) as u8);
            glob_clip_active.set(true);
            glob_clip_armed.set(true);
            glob_genre.set(CAPTURE_SLOT);
            glob_genre_fader.set(genre_fader_center(CAPTURE_SLOT, NUM_GENRES + 1));
            glob_mode_auto.set(true);
            glob_auto_running.set(true);
            glob_capture_flash.set(255);

            storage.modify_and_save(|s| {
                s.clip_active = true;
                s.clip_len = n;
                s.clip_bars = bars.min(255) as u8;
                s.clip_deg = degs;
                s.clip_on = ons;
                s.clip_dur = durs;
                s.mode_auto = true;
                s.auto_running = true;
            });
            true
        };

        loop {
            let (_chan, shift) = buttons.wait_for_any_down().await;
            long_press_fired.set(false);

            if shift {
                // Short vs long decided on release (Long must not also fire Short).
                let _ = buttons.wait_for_up(0).await;
                if long_press_fired.get() {
                    // Shift+Long: Panic in Auto (see long_press). Ignore in Perform.
                    continue;
                }
                if glob_mode_auto.get() {
                    // Auto + Shift+Tap → back to Perform.
                    glob_mode_auto.set(false);
                    chord_held.set(false);
                    pending_release.set(true);
                    storage.modify_and_save(|s| {
                        s.mode_auto = false;
                        s.auto_running = glob_auto_running.get();
                    });
                } else {
                    // Perform + Shift+Tap → capture last N bars.
                    let _ = commit_capture();
                }
                continue;
            }

            if glob_mode_auto.get() {
                // Auto: short = play/pause chord autoplay (global clock is Scene+Shift only)
                buttons.wait_for_up(0).await;
                if long_press_fired.get() {
                    // Long already handled (reseed genre / clear capture)
                    continue;
                }
                let playing = !glob_auto_running.get();
                glob_auto_running.set(playing);
                if !playing {
                    pending_release.set(true);
                }
                storage.modify_and_save(|s| s.auto_running = playing);
            } else {
                // Perform: hold = chord; release = note off.
                let count = glob_perform_count.get().max(1) as usize;
                let idx = value_to_index(glob_scrub.get(), count);
                let (deg, oct) = apply_perform_index(
                    idx,
                    &perform_deg_cell.get(),
                    &perform_oct_cell.get(),
                    glob_perform_count.get(),
                );
                glob_perform_idx.set(idx as u8);
                glob_degree.set(deg);
                glob_octave.set(oct);
                chord_held.set(true);

                buttons.wait_for_up(0).await;
                chord_held.set(false);
            }
        }
    };

    let long_press = async {
        loop {
            let (_chan, shift) = buttons.wait_for_any_long_press().await;
            long_press_fired.set(true);

            if shift {
                if glob_mode_auto.get() {
                    // Auto Shift+Long: Panic (All Notes/Sound Off)
                    panic_flag.set(true);
                    glob_capture_flash.set(160); // visible ack (was silent)
                }
                // Perform Shift+Long: unused — capture is Shift+Tap.
                continue;
            }

            if glob_mode_auto.get() {
                // Auto Long: clear capture + seed a fresh progression in this genre.
                glob_clip_active.set(false);
                glob_clip_armed.set(false);
                pending_release.set(true);
                let g = glob_last_genre.get().min(NUM_GENRES - 1);
                glob_genre.set(g);
                glob_genre_fader.set(genre_fader_center(g, NUM_GENRES));
                let mut slots = slots_cell.get();
                let n = seed_genre_into(&mut slots, g, &die);
                slots_cell.set(slots);
                glob_slot_count.set(n);
                refresh_perform_map(&slots, n);
                {
                    let count = n.max(1) as usize;
                    let idx = value_to_index(glob_scrub.get(), count);
                    glob_slot_idx.set(idx as u8);
                }
                storage.modify_and_save(|s| {
                    s.clip_active = false;
                    s.clip_len = 0;
                    s.genre = g as u8;
                    s.slots = slots;
                    s.slot_count = n;
                    s.swing = swing_from_genre(g);
                });
                glob_swing.set(swing_from_genre(g));
                glob_capture_flash.set(96);
            }
            // Perform Long: no-op — chord stays held for as long as the button is down.
        }
    };

    let fader_handler = async {
        let mut latch = app.make_latch(fader.get_value());
        loop {
            fader.wait_for_change().await;
            let layer = glob_latch.get();
            let target = match layer {
                LatchLayer::Main => {
                    if glob_mode_auto.get() {
                        storage.query(|s| s.tension)
                    } else {
                        storage.query(|s| s.scrub)
                    }
                }
                LatchLayer::Alt => glob_genre_fader.get(),
                LatchLayer::Third => storage.query(|s| s.div_fader),
            };
            if let Some(new_value) = latch.update(fader.get_value(), layer, target) {
                match layer {
                    LatchLayer::Main => {
                        if glob_mode_auto.get() {
                            glob_tension.set(new_value);
                            storage.modify_and_save(|s| s.tension = new_value);
                        } else {
                            glob_scrub.set(new_value);
                            let count = glob_perform_count.get().max(1) as usize;
                            let idx = value_to_index(new_value, count);
                            let (deg, oct) = apply_perform_index(
                                idx,
                                &perform_deg_cell.get(),
                                &perform_oct_cell.get(),
                                glob_perform_count.get(),
                            );
                            glob_perform_idx.set(idx as u8);
                            glob_degree.set(deg);
                            glob_octave.set(oct);
                            storage.modify_and_save(|s| s.scrub = new_value);
                        }
                    }
                    LatchLayer::Alt => {
                        // Keep continuous fader value as latch target (symmetric up/down).
                        glob_genre_fader.set(new_value);
                        let picks = if glob_clip_len.get() > 0 {
                            NUM_GENRES + 1
                        } else {
                            NUM_GENRES
                        };
                        let g = value_to_index(new_value, picks);
                        if g == CAPTURE_SLOT {
                            if glob_clip_len.get() > 0 {
                                glob_genre.set(CAPTURE_SLOT);
                                glob_clip_active.set(true);
                                glob_clip_armed.set(true);
                                pending_release.set(true);
                                storage.modify_and_save(|s| s.clip_active = true);
                            }
                        } else if g != glob_last_genre.get() || glob_clip_active.get() {
                            glob_last_genre.set(g);
                            glob_genre.set(g);
                            glob_clip_active.set(false);
                            glob_clip_armed.set(false);
                            pending_release.set(true);
                            let mut slots = slots_cell.get();
                            let n = seed_genre_into(&mut slots, g, &die);
                            slots_cell.set(slots);
                            glob_slot_count.set(n);
                            refresh_perform_map(&slots, n);
                            {
                                let count = n.max(1) as usize;
                                let idx = value_to_index(glob_scrub.get(), count);
                                glob_slot_idx.set(idx as u8);
                            }
                            storage.modify_and_save(|s| {
                                s.genre = g as u8;
                                s.slots = slots;
                                s.slot_count = n;
                                s.clip_active = false;
                                s.swing = swing_from_genre(g);
                            });
                            glob_swing.set(swing_from_genre(g));
                            // Keep ParamStore in sync so Scopepunk / configurator
                            // GetAppParams reflects the hardware genre pick.
                            params.update(|p| p.genre = g).await;
                        }
                    }
                    LatchLayer::Third => {
                        // Auto Feel: laid back (0) ↔ advanced (4095).
                        glob_feel.set(new_value);
                        storage.modify_and_save(|s| s.div_fader = new_value);
                    }
                }
            }
        }
    };

    let led_handler = async {
        let mut prev_gate_high = false;
        let mut last_pitch_counts = u16::MAX;
        loop {
            app.delay_millis(1).await;

            // CV Out: root of the current chord degree (always tracks selection).
            if let Some(ref jack) = out_jack {
                let degree = glob_degree.get();
                let octave = glob_octave.get();
                let notes = build_chord(root_midi, key, degree, voicing, octave);
                if let Some(&n) = notes.first() {
                    let counts = note_to_pitch(n).as_counts(range, vpo);
                    if counts != last_pitch_counts {
                        jack.set_value(counts);
                        last_pitch_counts = counts;
                    }
                }
            }

            if let Some(ref jack) = in_jack {
                let in_val = attenuate_bipolar(jack.get_value(), cv_att);
                glob_cv_val.set(in_val);
                if cv_dest == DEST_PANIC {
                    let high = in_val >= TRIG_HIGH;
                    if high && !prev_gate_high {
                        panic_flag.set(true);
                    }
                    prev_gate_high = high;
                } else {
                    prev_gate_high = false;
                    // Macro: Perform scrub / Auto tension (tension applied at clock read).
                    if !glob_mode_auto.get() {
                        let scrub = mod_u16(glob_scrub.get(), in_val);
                        let count = glob_perform_count.get().max(1) as usize;
                        let idx = value_to_index(scrub, count);
                        let (deg, oct) = apply_perform_index(
                            idx,
                            &perform_deg_cell.get(),
                            &perform_oct_cell.get(),
                            glob_perform_count.get(),
                        );
                        glob_perform_idx.set(idx as u8);
                        glob_degree.set(deg);
                        glob_octave.set(oct);
                    }
                }
            } else {
                prev_gate_high = false;
                glob_cv_val.set(2047);
            }
            let layer = if buttons.is_shift_pressed() && !buttons.is_button_pressed(0) {
                LatchLayer::Alt
            } else if !buttons.is_shift_pressed() && buttons.is_button_pressed(0) {
                LatchLayer::Third
            } else {
                LatchLayer::Main
            };
            glob_latch.set(layer);

            let flash = glob_capture_flash.get();
            let loop_flash = glob_loop_flash.get();
            if flash > 0 {
                leds.set(0, Led::Button, Color::White, Brightness::Custom(flash));
            } else if loop_flash > 0 && glob_mode_auto.get() && glob_clip_active.get() {
                leds.set(0, Led::Button, Color::White, Brightness::Custom(loop_flash));
            }

            match layer {
                LatchLayer::Main => {
                    let (slot, count) = if glob_mode_auto.get() {
                        (
                            glob_slot_idx.get() as u16,
                            glob_slot_count.get().max(1) as u16,
                        )
                    } else {
                        (
                            glob_perform_idx.get() as u16,
                            glob_perform_count.get().max(1) as u16,
                        )
                    };
                    let meter = (slot * 4095) / count.max(1);
                    let led = split_unsigned_value(meter);
                    let pulse = glob_activity.get();
                    // Perform = app color; Auto = orange (bright=play, dim=pause).
                    let (fader_color, button_color, button_bright) = if glob_mode_auto.get() {
                        if glob_auto_running.get() {
                            (Color::Orange, Color::Orange, Brightness::High)
                        } else {
                            (Color::Orange, Color::Orange, Brightness::Low)
                        }
                    } else {
                        (led_color, led_color, LED_BRIGHTNESS)
                    };
                    leds.set(
                        0,
                        Led::Top,
                        fader_color,
                        Brightness::Custom(led[0].max(pulse)),
                    );
                    leds.set(
                        0,
                        Led::Bottom,
                        fader_color,
                        Brightness::Custom(led[1].max(pulse / 2)),
                    );
                    if flash == 0 && loop_flash == 0 {
                        leds.set(0, Led::Button, button_color, button_bright);
                    }
                }
                LatchLayer::Alt => {
                    // Live fader preview so colors track both directions across full travel.
                    let picks = if glob_clip_len.get() > 0 {
                        NUM_GENRES + 1
                    } else {
                        NUM_GENRES
                    };
                    let fader_now = fader.get_value();
                    let g = value_to_index(fader_now, picks);
                    let color = if g == CAPTURE_SLOT {
                        CAPTURE_COLOR
                    } else {
                        GENRE_COLORS[g.min(NUM_GENRES - 1)]
                    };
                    let led = split_unsigned_value(fader_now);
                    leds.set(0, Led::Top, color, Brightness::Custom(led[0]));
                    leds.set(0, Led::Bottom, color, Brightness::Custom(led[1]));
                    if flash == 0 {
                        leds.set(0, Led::Button, color, Brightness::High);
                    }
                }
                LatchLayer::Third => {
                    if glob_mode_auto.get() {
                        // Auto+Hold: Rose, brightness = position in the auto progression.
                        let slot = glob_slot_idx.get() as u16;
                        let count = glob_slot_count.get().max(1) as u16;
                        let meter = (slot * 4095) / count.max(1);
                        let led = split_unsigned_value(meter);
                        leds.set(0, Led::Top, Color::Rose, Brightness::Custom(led[0]));
                        leds.set(0, Led::Bottom, Color::Rose, Brightness::Custom(led[1]));
                        if flash == 0 {
                            let bright = ((slot * 255) / count.max(1)) as u8;
                            leds.set(
                                0,
                                Led::Button,
                                Color::Rose,
                                Brightness::Custom(bright.max(16)),
                            );
                        }
                    } else {
                        // Perform+Hold: Blue, brightness tracks triggered chord root pitch.
                        let notes = build_chord(
                            root_midi,
                            key,
                            glob_degree.get(),
                            voicing,
                            glob_octave.get(),
                        );
                        let note = notes.first().copied().unwrap_or(root_midi);
                        let meter = (u16::from(note) * 4095) / 127;
                        let led = split_unsigned_value(meter);
                        leds.set(0, Led::Top, Color::Blue, Brightness::Custom(led[0]));
                        leds.set(0, Led::Bottom, Color::Blue, Brightness::Custom(led[1]));
                        if flash == 0 {
                            let bright = ((u16::from(note) * 255) / 127) as u8;
                            leds.set(
                                0,
                                Led::Button,
                                Color::Blue,
                                Brightness::Custom(bright.max(16)),
                            );
                        }
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
                    let (
                        slots,
                        count,
                        genre,
                        scrub,
                        tension,
                        div_f,
                        auto,
                        meander,
                        running,
                        clip_active,
                        clip_len,
                        clip_bars,
                        clip_deg,
                        clip_on,
                        clip_dur,
                        swing,
                    ) = storage.query(|s| {
                        (
                            s.slots,
                            s.slot_count,
                            s.genre as usize,
                            s.scrub,
                            s.tension,
                            s.div_fader,
                            s.mode_auto,
                            s.meander,
                            s.auto_running,
                            s.clip_active,
                            s.clip_len,
                            s.clip_bars,
                            s.clip_deg,
                            s.clip_on,
                            s.clip_dur,
                            s.swing,
                        )
                    });
                    slots_cell.set(slots);
                    glob_slot_count.set(count.max(1));
                    refresh_perform_map(&slots, count.max(1));
                    let g = genre.min(NUM_GENRES - 1);
                    glob_last_genre.set(g);
                    glob_scrub.set(scrub);
                    glob_tension.set(tension);
                    glob_feel.set(div_f);
                    glob_swing.set(swing.min(100));
                    glob_mode_auto.set(auto);
                    glob_meander.set(meander);
                    glob_auto_running.set(running);
                    clip_deg_cell.set(clip_deg);
                    clip_on_cell.set(clip_on);
                    clip_dur_cell.set(clip_dur);
                    glob_clip_len.set(clip_len);
                    glob_clip_bars.set(clip_bars.max(1));
                    let use_clip = clip_active && clip_len > 0;
                    glob_clip_active.set(use_clip);
                    glob_clip_armed.set(use_clip);
                    let genre_now = if use_clip { CAPTURE_SLOT } else { g };
                    let picks_now = if clip_len > 0 {
                        NUM_GENRES + 1
                    } else {
                        NUM_GENRES
                    };
                    glob_genre.set(genre_now);
                    glob_genre_fader.set(genre_fader_center(genre_now, picks_now));
                    panic_flag.set(true);
                }
                SceneEvent::SaveScene(scene) => {
                    storage.save_to_scene(scene).await;
                }
            }
        }
    };

    join(
        long_press,
        join(
            clock_watch,
            join5(
                engine,
                button_handler,
                fader_handler,
                led_handler,
                scene_handler,
            ),
        ),
    )
    .await;
}
