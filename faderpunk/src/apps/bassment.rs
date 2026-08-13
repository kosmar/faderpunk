//! Bassment — multi-genre monophonic basslines with bassist Voice personas.
//!
//! Shares the Grooves / Chord Vamp genre axis (`genre_palette` + `led_fx` +
//! `groove` swing/feel). UX mirrors Grooves; pitch CV follows Chord Vamp.

use embassy_futures::{
    join::{join, join3, join5},
    select::{select, select3},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use heapless::Vec;
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue,
    latch::LatchLayer,
    quantizer::Pitch,
    utils::{attenuate_bipolar, split_unsigned_value, value_to_index},
    AppIcon, Brightness, Color, Config, Key, MidiCc, MidiChannel, MidiNote, MidiOut, Note,
    Param, Range, Value, VoltPerOct, APP_MAX_PARAMS,
};

use crate::app::{
    App, AppParams, AppStorage, Die, Led, ManagedStorage, ParamStore, SceneEvent,
};
use crate::apps::follow_key;
use crate::apps::genre_palette::{genre_fader_center, GENRE_NAMES, GENRE_PROG_8, NUM_GENRES};
use crate::apps::groove::{
    bit_set, feel_curve, feel_lerp_i32, feel_lerp_u16, rot16, step_chance, swing_bias,
    swing_delay_ticks, FLAT_VEL, SIXTEENTH, STEPS_PER_BAR,
};
use crate::apps::led_fx::{genre_nearest, genre_pair, lerp_i32, lerp_u8, spectrum_color};
use crate::apps::ornament::{self, ArticContext, GrooveVoice, MAX_HITS};
use crate::tasks::global_config::get_global_config;

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 15;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
const BUTTON_DUCK_MS: u16 = 25;
const FADER_MOVE_THRESH: u16 = 64;
const CLOCK_STALL_MS: u16 = 100;
const VOICE_FLASH_MS: u16 = 300;

/// Jack duty and its modifier in one param, as in Grooves: a CV In destination
/// and CV Out are mutually exclusive, so they share a single enum.
const JACK_OUT: usize = 0;
const JACK_IN_DENSITY: usize = 1;
const JACK_IN_FEEL: usize = 2;
const JACK_IN_RESET: usize = 3;
const JACK_COUNT: usize = 4;

const TRIG_HIGH: u16 = 2458;

const NUM_VOICES: usize = 8;

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

/// Cycle / Enum order = historical era narrative (not birth-year pedantry).
/// Mingus → Motown → P-Funk → fusion → dub duo → alt-funk → quirk.
const VOICE_NAMES: &[&str] = &[
    "Mingus",
    "Jamerson",
    "Bootsy",
    "Jaco",
    "Robbie",
    "Flabba",
    "Flea",
    "Claypool",
];

/// Per-genre monophonic bass DNA (16 sixteenths).
struct BassPattern {
    hits: u16,
    hits_fill: u16,
    accent_mask: u16,
    base_vel: u8,
    accent_vel: u8,
    timing: [i8; 16],
    degree: [i8; 16],
    oct_off: [i8; 16],
    gate_w: [u8; 16],
}

/// Bassist persona — transforms applied after genre DNA.
#[derive(Clone, Copy)]
struct VoiceProfile {
    /// Clamp |oct_off| to this (0..=2).
    oct_span: i8,
    /// Chance (0–100) to pull a chromatic approach a semitone below the target.
    approach_pct: u8,
    /// Chance (0–100) that a fill-only hit becomes a quiet ghost (or skip at low frac).
    ghost_pct: u8,
    /// Gate length scale (0–100); lower = more staccato.
    staccato: u8,
    /// Extra ghost-chance bias in 0..=1600 (added to ghost_pct after feel scaling).
    syncop_bias: u16,
    /// Accent velocity boost 0..=55.
    accent_boost: u8,
    /// Prefer root/fifth: fold odd degrees toward 0 or 4.
    pocket: bool,
}

const VOICES: [VoiceProfile; NUM_VOICES] = [
    // Mingus — chromatic approach, odd accents (bebop / post-bop)
    VoiceProfile {
        oct_span: 1,
        approach_pct: 95,
        ghost_pct: 55,
        staccato: 68,
        syncop_bias: 480,
        accent_boost: 32,
        pocket: false,
    },
    // Jamerson — locked Motown pocket (root/fifth only, no leaps)
    VoiceProfile {
        oct_span: 0,
        approach_pct: 0,
        ghost_pct: 2,
        staccato: 96,
        syncop_bias: 0,
        accent_boost: 2,
        pocket: true,
    },
    // Bootsy — funk ghosts + octave drops
    VoiceProfile {
        oct_span: 1,
        approach_pct: 40,
        ghost_pct: 96,
        staccato: 42,
        syncop_bias: 1200,
        accent_boost: 36,
        pocket: false,
    },
    // Jaco — melodic, sustained, approaches + octave leaps
    VoiceProfile {
        oct_span: 2,
        approach_pct: 88,
        ghost_pct: 12,
        staccato: 100,
        syncop_bias: 60,
        accent_boost: 8,
        pocket: false,
    },
    // Robbie — Shakespeare dub pocket: deep, sustained, sparse
    VoiceProfile {
        oct_span: 0,
        approach_pct: 0,
        ghost_pct: 4,
        staccato: 100,
        syncop_bias: 0,
        accent_boost: 3,
        pocket: true,
    },
    // Flabba — Holt roots walk: roomy gates, mild syncop (duo with Robbie)
    VoiceProfile {
        oct_span: 1,
        approach_pct: 22,
        ghost_pct: 28,
        staccato: 92,
        syncop_bias: 220,
        accent_boost: 16,
        pocket: false,
    },
    // Flea — short gates, max syncop, hard accents, octave jumps
    VoiceProfile {
        oct_span: 2,
        approach_pct: 28,
        ghost_pct: 72,
        staccato: 16,
        syncop_bias: 1600,
        accent_boost: 48,
        pocket: false,
    },
    // Claypool — quirky leaps, chromatic nudges, odd staccato
    VoiceProfile {
        oct_span: 2,
        approach_pct: 78,
        ghost_pct: 68,
        staccato: 24,
        syncop_bias: 1100,
        accent_boost: 44,
        pocket: false,
    },
];

/// Distinct flash colors so Long Voice cycle is obvious on the button.
const VOICE_FLASH_COLOR: [Color; NUM_VOICES] = [
    Color::Violet, // Mingus
    Color::Orange, // Jamerson
    Color::Pink,   // Bootsy
    Color::Cyan,   // Jaco
    Color::Green,  // Robbie
    Color::Lime,   // Flabba
    Color::Red,    // Flea
    Color::Yellow, // Claypool
];

/// Phrase length in bars (rhythm + harmony cycle together).
const PHRASE_BARS: u32 = 8;

/// Rhythm displacement: keep the motif repeating; only the answer moves.
/// Form ≈ AA B A′ — repetition with variation, not a new line every bar.
fn phrase_rot(phrase_bar: u32) -> u32 {
    match phrase_bar % PHRASE_BARS {
        4 | 5 => 4, // B: answered groove (one-beat shift)
        _ => 0,     // A / A′: same rhythmic motif
    }
}

/// Melodic read index: same contour on A bars; answered contour on B.
fn phrase_degree_si(step: u32, phrase_bar: u32) -> usize {
    let base = (step % STEPS_PER_BAR) as usize;
    match phrase_bar % PHRASE_BARS {
        4 | 5 => (base + 4) % 16,
        6 => (base + 2) % 16, // A′: slight ornament
        _ => base,
    }
}

const PATTERNS: [BassPattern; NUM_GENRES] = [
    // 0 Dub — sparse half-time; fill densifies toward walking 8ths/16ths
    BassPattern {
        hits: 0b0000_0001_0000_0001,
        hits_fill: 0b1010_1010_1010_1010,
        accent_mask: 0b0000_0000_0000_0001,
        base_vel: 70,
        accent_vel: 105,
        timing: [0, 1, 0, 2, 1, 2, 0, 2, 0, 1, 0, 2, 1, 2, 0, 2],
        degree: [0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 4, 0, 0, 0],
        oct_off: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        gate_w: [90, 50, 50, 50, 50, 50, 70, 50, 85, 50, 50, 50, 75, 50, 50, 50],
    },
    // 1 Disco — four-on-floor roots with octave pops
    BassPattern {
        hits: 0b0001_0001_0001_0001,
        hits_fill: 0b1100_0100_0100_0100,
        accent_mask: 0b0001_0000_0001_0000,
        base_vel: 75,
        accent_vel: 110,
        timing: [0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2],
        degree: [0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 4, 0, 0, 3, 0],
        oct_off: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
        gate_w: [70, 40, 70, 45, 70, 40, 70, 50, 70, 40, 70, 45, 65, 40, 60, 40],
    },
    // 2 House — classic pump i–VII–VI–VII
    BassPattern {
        hits: 0b0001_0001_0001_0001,
        hits_fill: 0b0100_0100_1100_0100,
        accent_mask: 0b0001_0001_0001_0001,
        base_vel: 80,
        accent_vel: 112,
        timing: [0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2, 0, 2],
        degree: [0, 0, 0, 0, 6, 0, 6, 0, 5, 0, 5, 0, 6, 0, 6, 0],
        oct_off: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        gate_w: [75, 40, 70, 40, 75, 40, 70, 40, 75, 40, 70, 40, 75, 40, 70, 40],
    },
    // 3 Techno — minimal root drones + sparse fifths
    BassPattern {
        hits: 0b0001_0001_0001_0001,
        hits_fill: 0b0100_0100_0100_0100,
        accent_mask: 0b0001_0000_0001_0000,
        base_vel: 85,
        accent_vel: 108,
        timing: [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1],
        degree: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0],
        oct_off: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        gate_w: [95, 40, 90, 40, 95, 40, 90, 40, 95, 40, 90, 55, 95, 40, 90, 40],
    },
    // 4 Trip-Hop — laid-back sparse line
    BassPattern {
        hits: 0b0000_0001_0000_0001,
        hits_fill: 0b1001_1000_1001_1000,
        accent_mask: 0b0000_0000_0000_0001,
        base_vel: 60,
        accent_vel: 95,
        timing: [0, 2, 1, 3, 2, 3, 1, 3, 0, 2, 1, 3, 3, 4, 2, 3],
        degree: [0, 0, 0, 0, 0, 0, 6, 0, 0, 0, 0, 0, 5, 0, 4, 0],
        oct_off: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, -1, 0],
        gate_w: [100, 50, 50, 50, 50, 50, 80, 50, 90, 50, 50, 50, 85, 50, 70, 50],
    },
    // 5 Hip-Hop — boom-bap walk
    BassPattern {
        hits: 0b0100_0001_0010_0001,
        hits_fill: 0b1000_0110_0100_0110,
        accent_mask: 0b0000_0001_0000_0001,
        base_vel: 70,
        accent_vel: 110,
        timing: [0, 2, 1, 3, 0, 2, 1, 3, 0, 2, 1, 3, 1, 3, 2, 4],
        degree: [0, 0, 2, 0, 0, 0, 5, 0, 0, 0, 3, 0, 0, 0, 6, 0],
        oct_off: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        gate_w: [80, 40, 55, 40, 75, 40, 60, 40, 80, 40, 55, 40, 70, 40, 65, 40],
    },
    // 6 Jungle — busy amen energy
    BassPattern {
        hits: 0b0100_1001_0010_0101,
        hits_fill: 0b1010_0100_1001_0010,
        accent_mask: 0b0000_0001_0000_0001,
        base_vel: 65,
        accent_vel: 108,
        timing: [0, 3, -1, 2, 1, 3, 0, 4, 0, 3, -1, 2, 1, 3, 0, 4],
        degree: [0, 0, 6, 0, 0, 5, 0, 2, 0, 0, 6, 0, 5, 0, 2, 0],
        oct_off: [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
        gate_w: [55, 35, 50, 35, 60, 40, 50, 45, 55, 35, 50, 35, 60, 40, 50, 40],
    },
    // 7 UK Garage — skippy 2-step bass
    BassPattern {
        hits: 0b1000_1001_0010_0001,
        hits_fill: 0b0110_0100_1001_0010,
        accent_mask: 0b0000_0001_0000_0001,
        base_vel: 70,
        accent_vel: 110,
        timing: [0, 3, -1, 2, 0, 3, 1, 4, 0, 3, -1, 2, 0, 3, 1, 4],
        degree: [0, 0, 2, 0, 0, 0, 5, 0, 0, 0, 2, 0, 0, 5, 6, 0],
        oct_off: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
        gate_w: [50, 35, 60, 35, 55, 35, 65, 40, 50, 35, 60, 35, 55, 45, 50, 35],
    },
    // 8 Dubstep — half-time wobble roots
    BassPattern {
        hits: 0b0000_0000_0000_0001,
        hits_fill: 0b1000_1000_1100_1000,
        accent_mask: 0b0000_0000_0000_0001,
        base_vel: 80,
        accent_vel: 115,
        timing: [0, 1, 0, 2, 1, 2, 0, 2, 0, 1, 0, 2, 1, 2, 0, 2],
        degree: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 6, 0, 0],
        oct_off: [-1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        gate_w: [100, 40, 40, 40, 40, 40, 40, 40, 90, 40, 70, 40, 40, 75, 40, 40],
    },
];

/// Filter brightness per genre, lerped on the shared spectrum like the
/// velocities: dub and dubstep sit deep, garage and jungle cut.
const GENRE_CUTOFF: [u16; NUM_GENRES] = [
    700,  // Dub — deep and dark
    1900, // Disco — round but present
    1600, // House
    1400, // Techno — mid-dark, resonant
    900,  // Trip-Hop — dusty
    1100, // Hip-Hop — boom-bap
    2100, // Jungle — snappy
    2300, // UK Garage — brightest, skippy
    800,  // Dubstep — low wobble
];

const FILL_VARIANTS: usize = 3;
/// Density at which Shift+Tap reads as a pedal break instead of an additive fill.
const BREAK_DENSITY: u16 = 2600;
/// Pocket voices flip to pedal earlier — they don't pile on.
const BREAK_DENSITY_POCKET: u16 = 1800;

/// End-weighted fill rhythms (bit N = 16th step N). Build toward the downbeat.
const FILL_RHYTHM: [[u16; FILL_VARIANTS]; NUM_GENRES] = [
    // 0 Dub — sparse late answers
    [
        0b1100_0000_0000_0000,
        0b1010_0000_0000_0000,
        0b1000_1000_0000_0000,
    ],
    // 1 Disco — octave-pop run into the one
    [
        0b1110_1010_0000_0000,
        0b1101_0100_0001_0000,
        0b1111_0000_0001_0000,
    ],
    // 2 House — driving 8ths into the bar line
    [
        0b1110_1010_1010_0000,
        0b1101_0101_0001_0000,
        0b1111_0100_0100_0000,
    ],
    // 3 Techno — straight machine climb
    [
        0b1111_0101_0101_0000,
        0b1110_1110_0000_0000,
        0b1111_1111_0000_0000,
    ],
    // 4 Trip-Hop — dragging late figures
    [
        0b1100_0000_0000_0000,
        0b1010_0000_1000_0000,
        0b1101_0000_0000_0000,
    ],
    // 5 Hip-Hop — boom-bap walk into the one
    [
        0b1110_0100_1000_0000,
        0b1101_0010_0100_0000,
        0b1111_0000_1001_0000,
    ],
    // 6 Jungle — amen chops, busy late
    [
        0b1111_1010_0100_1000,
        0b1101_0110_1001_0000,
        0b1110_1101_0010_0000,
    ],
    // 7 UK Garage — skippy syncopation
    [
        0b1110_0101_0010_0000,
        0b1101_1000_0100_1000,
        0b1111_0010_1000_0000,
    ],
    // 8 Dubstep — half-time weight, few hits
    [
        0b1000_0000_0000_0000,
        0b1100_0000_1000_0000,
        0b1010_0000_0000_0000,
    ],
];

/// Full-bar solo rhythms: denser than fills, but with gaps for breath (not a thunderstorm).
const SOLO_RHYTHM: [[u16; FILL_VARIANTS]; NUM_GENRES] = [
    // 0 Dub — echo answers, still air
    [
        0b1001_0000_1001_0000,
        0b1010_0100_1000_0100,
        0b1000_1001_0000_1000,
    ],
    // 1 Disco — continuous 8th chatter
    [
        0b1010_1010_1010_1010,
        0b1101_0101_1101_0101,
        0b1010_1110_1010_1110,
    ],
    // 2 House — pump with skips
    [
        0b1010_1010_1010_1001,
        0b1101_0101_0101_0100,
        0b1010_1101_1010_1100,
    ],
    // 3 Techno — straight but not every 16th
    [
        0b1010_1010_1010_1010,
        0b1110_1110_1110_1110,
        0b1101_1101_1101_1101,
    ],
    // 4 Trip-Hop — sparse dragging phrases
    [
        0b1000_0100_1000_0100,
        0b1010_0000_1001_0000,
        0b1000_1000_0100_1000,
    ],
    // 5 Hip-Hop — walking solo
    [
        0b1010_0101_1010_0101,
        0b1101_0010_1101_0010,
        0b1010_1001_0100_1010,
    ],
    // 6 Jungle — busy with breath holes
    [
        0b1011_0101_1010_1001,
        0b1101_1010_0101_1010,
        0b1010_1101_1011_0100,
    ],
    // 7 UK Garage — skippy full-bar
    [
        0b1010_0101_1010_0101,
        0b1101_0010_1101_0010,
        0b1001_0110_1001_0110,
    ],
    // 8 Dubstep — half-time weight with answers
    [
        0b1000_0100_1000_1000,
        0b1010_0000_1001_0000,
        0b1000_1000_0100_1000,
    ],
];

/// Melodic contour for a fill/solo gesture. Locked at arm so a tap stays one phrase.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FillShape {
    WalkUp,
    WalkDown,
    ArpClimb,
    OctavePush,
    Pedal,
}

/// Deterministic 0..99 hash so low Feel keeps one signature figure per genre.
fn fill_hash(genre: usize) -> u8 {
    let x = (genre as u32)
        .wrapping_mul(2_654_435_761)
        .wrapping_add(0x9E37_79B9);
    ((x >> 9) % 100) as u8
}

/// Feel-weighted variant roll: low Feel stays hash-locked, high Feel leans on the Die.
fn fill_variant(die: &Die, genre: usize, feel: u16) -> usize {
    let hashed = u32::from(fill_hash(genre));
    let live = u32::from(die.roll() % 100);
    let w = (u32::from(feel_curve(feel)) * 4) / 5;
    let roll = (hashed * (4095 - w) + live * w) / 4095;
    ((roll as usize * FILL_VARIANTS) / 100).min(FILL_VARIANTS - 1)
}

/// Fill velocity: crescendo from `base` toward `accent` across the bar.
fn fill_vel_pct(base: u8, accent: u8, step: u32) -> u16 {
    let b = u16::from(base);
    let a = u16::from(accent).max(b);
    let idx = (step % STEPS_PER_BAR) as u16;
    b + ((a - b) * idx) / (STEPS_PER_BAR as u16 - 1)
}

/// Solo velocity: Feel-weighted Die chatter between `base` and `accent`.
fn solo_vel_pct(base: u8, accent: u8, die: &Die, feel: u16) -> u16 {
    let b = u16::from(base);
    let a = u16::from(accent).max(b);
    let f = u32::from(feel_curve(feel));
    let mid = (b + a) / 2;
    let span = (a - b).max(1);
    let live = b + ((span as u32 * u32::from(die.roll() % 100)) / 99) as u16;
    ((u32::from(mid) * (4095 - f) + u32::from(live) * f) / 4095) as u16
}

/// Voice × genre picks the melodic contour. Pocket / Dub lean Pedal; busy voices walk/arp.
fn fill_shape(voice: &VoiceProfile, genre: usize, die: &Die, feel: u16) -> FillShape {
    let gfeel = feel_curve(feel);
    let roll = {
        let hashed = u32::from(fill_hash(genre.wrapping_add(voice.oct_span as usize)));
        let live = u32::from(die.roll() % 100);
        let w = (u32::from(gfeel) * 4) / 5;
        ((hashed * (4095 - w) + live * w) / 4095) as u8
    };
    // Dub / Dubstep / pocket: almost never pile on.
    if voice.pocket || matches!(genre, 0 | 8) {
        return if roll < 70 {
            FillShape::Pedal
        } else if voice.oct_span >= 1 && roll < 90 {
            FillShape::OctavePush
        } else {
            FillShape::WalkUp
        };
    }
    if voice.approach_pct >= 80 {
        // Mingus / Jaco — chromatic or melodic walks.
        return if roll < 45 {
            FillShape::WalkUp
        } else if roll < 75 {
            FillShape::WalkDown
        } else {
            FillShape::ArpClimb
        };
    }
    if voice.oct_span >= 2 {
        // Flea / Claypool / Jaco — leaps and arps.
        return if roll < 40 {
            FillShape::ArpClimb
        } else if roll < 70 {
            FillShape::OctavePush
        } else {
            FillShape::WalkUp
        };
    }
    if voice.ghost_pct >= 80 {
        // Bootsy — octave pops.
        return if roll < 60 {
            FillShape::OctavePush
        } else {
            FillShape::WalkUp
        };
    }
    // Default: walk with occasional arp.
    if roll < 50 {
        FillShape::WalkUp
    } else if roll < 80 {
        FillShape::WalkDown
    } else {
        FillShape::ArpClimb
    }
}

/// Index of this hit among fill-mask hits in the bar, and total hit count.
fn fill_hit_index(mask: u16, step: u32) -> (usize, usize) {
    let total = mask.count_ones() as usize;
    if total == 0 {
        return (0, 0);
    }
    let s = step % STEPS_PER_BAR;
    let mut idx = 0usize;
    for i in 0..STEPS_PER_BAR {
        if bit_set(mask, i) {
            if i == s {
                return (idx, total);
            }
            idx += 1;
        }
    }
    (0, total)
}

/// True when this step is the last fill hit in the bar (leading-tone seat).
fn is_last_fill_hit(mask: u16, step: u32) -> bool {
    let (idx, total) = fill_hit_index(mask, step);
    total > 0 && idx + 1 == total
}

/// Melodic degree for a fill/solo step. Last hit = leading tone into `next_chord`.
fn fill_degree(
    shape: FillShape,
    mask: u16,
    step: u32,
    cur_chord: i8,
    next_chord: i8,
) -> (i8, i8) {
    // Returns (degree 0..=6 relative to root after chord add, oct_off).
    let cur = cur_chord.rem_euclid(7);
    let next = next_chord.rem_euclid(7);
    if shape == FillShape::Pedal {
        return (cur, 0);
    }
    if is_last_fill_hit(mask, step) {
        // Leading tone: scale degree just below the target.
        let lead = (next + 6).rem_euclid(7);
        return (lead, 0);
    }
    let (idx, total) = fill_hit_index(mask, step);
    let denom = (total.saturating_sub(1)).max(1);
    let t = ((idx * 255) / denom) as u8; // 0..=255 along the phrase

    match shape {
        FillShape::WalkUp => {
            // Walk ascending scale degrees from cur toward / past next.
            let span = ((next + 7 - cur) % 7).max(1);
            let steps = 1 + ((u16::from(t) * (span as u16 + 1)) / 255) as i8;
            ((cur + steps).rem_euclid(7), 0)
        }
        FillShape::WalkDown => {
            let span = ((cur + 7 - next) % 7).max(1);
            let steps = 1 + ((u16::from(t) * (span as u16 + 1)) / 255) as i8;
            ((cur + 7 - steps).rem_euclid(7), 0)
        }
        FillShape::ArpClimb => {
            // Chord tones relative to current, climbing: root → 3rd → 5th → octave.
            let chord_tones = [0i8, 2, 4, 0];
            let i = ((u16::from(t) * 3) / 255) as usize;
            let rel = chord_tones[i.min(3)];
            let oct = if i >= 3 { 1 } else { 0 };
            ((cur + rel).rem_euclid(7), oct)
        }
        FillShape::OctavePush => {
            // Mostly current chord root; mid-phrase pops an octave.
            let oct = if t > 80 && t < 200 { 1 } else { 0 };
            let deg = if t > 140 && t < 180 {
                (cur + 4).rem_euclid(7) // fifth flick
            } else {
                cur
            };
            (deg, oct)
        }
        FillShape::Pedal => (cur, 0),
    }
}

/// Sustain look-ahead against the active fill/solo mask (not the groove DNA).
fn sustain_fill_sixteenths(mask: u16, step: u32, staccato: u8, is_break: bool) -> u32 {
    if is_break {
        // Pedal: hold nearly the whole bar.
        return 14;
    }
    let mut n = 1u32;
    for look in 1..8u32 {
        let s = (step + look) % STEPS_PER_BAR;
        // Don't wrap sustain across the bar line — fills resolve there.
        if step % STEPS_PER_BAR + look >= STEPS_PER_BAR {
            break;
        }
        if bit_set(mask, s) {
            break;
        }
        n += 1;
    }
    // Staccato voices shorten; sustained voices keep more of the gap.
    let scaled = (n * u32::from(staccato) / 100).max(1);
    scaled.min(n)
}

pub static CONFIG: Config<PARAMS> = Config::new(
    "Bassment",
    "Multi-genre basslines with bassist voices",
    Color::Cyan,
    AppIcon::NoteGrid,
)
.add_param(Param::MidiNote { name: "Root" })
.add_param(Param::Enum {
    name: "Scale",
    variants: SCALE_NAMES,
})
.add_param(Param::Enum {
    name: "Genre",
    variants: GENRE_NAMES,
})
.add_param(Param::Enum {
    name: "Voice",
    variants: VOICE_NAMES,
})
.add_param(Param::MidiChannel {
    name: "MIDI Channel",
})
.add_param(Param::MidiOut)
// A negative cap swings the offbeats early — the sign carries Swing Dir.
.add_param(Param::i32 {
    name: "Groove max %",
    min: -100,
    max: 100,
})
.add_param(Param::i32 {
    name: "GATE %",
    min: 1,
    max: 100,
})
.add_param(Param::Enum {
    name: "Jack",
    variants: &["CV Out", "CV In Density", "CV In Feel", "CV In Reset"],
})
.add_param(Param::Range {
    name: "Range",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::VoltPerOct)
.add_param(Param::i32 {
    name: "CV Att",
    min: 0,
    max: 100,
})
// 0 = off. Typical synth cutoff is 74; value follows bassline / fill / solo.
.add_param(Param::MidiCc { name: "Filter CC" })
// Following the device tonic turns the global Tonic fader into a live
// transpose that moves Bassment together with Contura, Arp and Venn.
.add_param(Param::bool {
    name: "Follow device tonic",
})
.add_param(Param::bool {
    name: "Follow device scale",
});

pub struct Params {
    root: MidiNote,
    scale: usize,
    genre: usize,
    voice: usize,
    midi_channel: MidiChannel,
    midi_out: MidiOut,
    /// Caps Third Feel: swing + microtiming + ghost drag + velocity contrast.
    /// The sign carries the swing direction, so a negative cap swings early.
    groove_max_pct: i32,
    gatel: i32,
    jack: usize,
    range: Range,
    vpo: VoltPerOct,
    cv_att: i32,
    /// Filter cutoff CC number; 0 disables CC output.
    filter_cc: MidiCc,
    follow_tonic: bool,
    follow_scale: bool,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        // Pre-Filter-CC blobs had twelve slots; accept them and default CC off.
        if values.len() < 12 {
            return None;
        }
        // The pre-merge layout carried the same types in these first twelve
        // slots and only added CV Dest and Swing Dir after them, so a stored
        // blob migrates forward on its own: CV Out stays CV Out, and CV In
        // lands on CV In Density, which was the old default destination.
        Some(Self {
            root: MidiNote::from_value(values[0]),
            scale: usize::from_value(values[1]).min(SCALE_NAMES.len() - 1),
            genre: usize::from_value(values[2]).min(NUM_GENRES - 1),
            voice: usize::from_value(values[3]).min(NUM_VOICES - 1),
            midi_channel: MidiChannel::from_value(values[4]),
            midi_out: MidiOut::from_value(values[5]),
            groove_max_pct: i32::from_value(values[6]).clamp(-100, 100),
            gatel: i32::from_value(values[7]).clamp(1, 100),
            jack: usize::from_value(values[8]).min(JACK_COUNT - 1),
            range: Range::from_value(values[9]),
            vpo: VoltPerOct::from_value(values[10]),
            cv_att: i32::from_value(values[11]).clamp(0, 100),
            filter_cc: if values.len() > 12 {
                MidiCc::from_value(values[12])
            } else {
                MidiCc::from(0u8)
            },
            // Older blobs predate the follow flags. Default them the way a
            // fresh instance does so a stored patch transposes along too.
            follow_tonic: if values.len() > 13 {
                bool::from_value(values[13])
            } else {
                true
            },
            follow_scale: values.len() > 14 && bool::from_value(values[14]),
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.root.into()).unwrap();
        vec.push(self.scale.into()).unwrap();
        vec.push(self.genre.into()).unwrap();
        vec.push(self.voice.into()).unwrap();
        vec.push(self.midi_channel.into()).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.groove_max_pct.into()).unwrap();
        vec.push(self.gatel.into()).unwrap();
        vec.push(self.jack.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.vpo.into()).unwrap();
        vec.push(self.cv_att.into()).unwrap();
        vec.push(self.filter_cc.into()).unwrap();
        vec.push(self.follow_tonic.into()).unwrap();
        vec.push(self.follow_scale.into()).unwrap();
        vec
    }
}

fn att_from_pct(pct: i32) -> u16 {
    ((pct.clamp(0, 100) as u32 * 4095) / 100) as u16
}

/// Filter cutoff as a 12-bit MIDI CC, built the way the rest of the line is:
/// a genre base lerped along the shared spectrum, the Voice persona's bite on
/// top, and the hit's own dynamics so accents open the filter. Fills lift,
/// solos bloom, the pedal break ducks away.
#[allow(clippy::too_many_arguments)]
fn filter_cutoff_12bit(
    g_lo: usize,
    g_hi: usize,
    g_frac: u8,
    voice: &VoiceProfile,
    vel_pct: u16,
    base_vel: u8,
    is_solo: bool,
    is_fill: bool,
    is_break: bool,
    feel: u16,
) -> u16 {
    let genre = lerp_i32(
        i32::from(GENRE_CUTOFF[g_lo]),
        i32::from(GENRE_CUTOFF[g_hi]),
        g_frac,
    );
    // Hard hitters cut through; pocket players stay warm and round.
    let persona = i32::from(voice.accent_boost) * 10 - if voice.pocket { 300 } else { 0 };
    // Accents open up — the defining move of a plucked or acid bass.
    let dynamics = (i32::from(vel_pct) - i32::from(base_vel)) * 18;
    let situation = if is_solo {
        1500
    } else if is_break {
        -600
    } else if is_fill {
        700
    } else {
        0
    };
    let feel_add = (i32::from(feel) * 300) / 4095;

    (genre + persona + dynamics + situation + feel_add).clamp(200, 4095) as u16
}

fn mod_u16(base: u16, in_val: u16) -> u16 {
    (base as i32 + in_val as i32 - 2047).clamp(0, 4095) as u16
}

#[derive(Serialize, Deserialize)]
pub struct Storage {
    feel: u16,
    density: u16,
    muted: bool,
    voice: u8,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            // Mid-high so Third already grooves out of the box.
            feel: 3400,
            density: 2048,
            muted: false,
            voice: 1, // Jamerson
        }
    }
}

impl AppStorage for Storage {}

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

fn ghost_vel_pct(frac: u8, quiet: u16, full: u16) -> u16 {
    quiet + ((full - quiet) as u32 * frac as u32 / 255) as u16
}

/// Extra microtiming ticks at high groove (beyond pattern DNA).
fn groove_timing_boost(feel: u16, groove_max_pct: i32, step: u32) -> i32 {
    let g = groove_feel(feel, groove_max_pct);
    let t = i32::from(feel_curve(g));
    // Odd 16ths push later; some even 16ths pull early — classic pocket.
    let signed = if step % 2 == 1 { 2 } else { -1 };
    (signed * t) / 4095
}

fn ghost_drag_ticks(density: u16, feel: u16, groove_max_pct: i32) -> u32 {
    let g = groove_feel(feel, groove_max_pct);
    let dens = (density as u32 * 3) / 4095;
    let feel_extra = (u32::from(feel_curve(g)) * 4) / 4095;
    (dens + feel_extra).min(6)
}

/// How many 16ths a hit can sustain before the next sounded step (1..=8).
fn sustain_sixteenths(hits: u16, hits_fill: u16, step: u32, density: u16) -> u32 {
    let mut n = 1u32;
    for look in 1..8u32 {
        let s = step + look;
        if bit_set(hits, s) || fill_reveal(hits_fill, density, s).is_some() {
            break;
        }
        n += 1;
    }
    n
}

fn midi_vel(mult: u16) -> u16 {
    ((4095u32 * mult as u32) / 100).min(4095) as u16
}

/// Effective Feel after Groove max % ceiling (0..=4095). Only the magnitude
/// caps — the sign of Groove max % picks the swing direction.
fn groove_cap_pct(groove_max_pct: i32) -> u32 {
    groove_max_pct.unsigned_abs().min(100)
}

fn groove_feel(feel: u16, groove_max_pct: i32) -> u16 {
    ((u32::from(feel) * groove_cap_pct(groove_max_pct)) / 100).min(4095) as u16
}

/// Swing % from genre bias × curved Feel, capped by Groove max %.
fn feel_swing_pct(bias: u8, feel: u16, groove_max_pct: i32) -> i32 {
    let g = groove_feel(feel, groove_max_pct);
    let f = u32::from(feel_curve(g));
    // Bias is a floor-ish character; at full groove allow up to Groove max.
    let from_bias = (u32::from(bias) * f) / 4095;
    let toward_max = (groove_cap_pct(groove_max_pct) * f) / 4095;
    // Blend: keep genre DNA but let Groove max open the ceiling.
    let pct = (from_bias * 2 + toward_max) / 3;
    pct.min(groove_cap_pct(groove_max_pct)) as i32
}

fn note_to_pitch(note: u8) -> Pitch {
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

fn fold_pocket(degree: i8) -> i8 {
    match degree.rem_euclid(7) {
        0 | 3 | 4 => degree.rem_euclid(7),
        1 | 2 => 0,
        _ => 4,
    }
}

/// Soft Voice × Genre coupling (no new params): pocket voices tame Jungle/UKG fills;
/// busy voices thin out in sparse Dub/Techno.
fn voice_genre_bias(voice: &VoiceProfile, genre: usize) -> (u8, u8) {
    let mut ghost = voice.ghost_pct;
    let mut approach = voice.approach_pct;
    match genre.min(NUM_GENRES - 1) {
        6 | 7 if voice.pocket => {
            // Jamerson/Robbie in Jungle / UK Garage — keep the pocket, cut ghosts.
            ghost /= 3;
            approach /= 2;
        }
        0 | 3 if !voice.pocket => {
            // Busy voices in Dub / Techno — less chatter on sparse DNA.
            ghost = ((ghost as u16 * 2) / 3) as u8;
        }
        5 | 6 if voice.approach_pct > 50 => {
            // Walking/approach voices lean into Hip-Hop / Jungle answers.
            approach = approach.saturating_add(12).min(100);
        }
        _ => {}
    }
    (ghost, approach)
}

/// Groove-weighted roll: low Feel stays hash-locked; high Feel leans on live Die.
fn chance_roll(die: &Die, step: u32, voice: usize, salt: u32, groove_t: u16) -> u8 {
    let hashed = u32::from(step_chance(step, voice, salt));
    let live = u32::from(die.roll() % 100);
    // Cap live mix at ~80% so genre DNA still peeks through at max Feel.
    let w = (u32::from(groove_t) * 4) / 5;
    ((hashed * (4095 - w) + live * w) / 4095) as u8
}

#[derive(Clone, Copy)]
struct ResolvedHit {
    note: u8,
    vel_pct: u16,
    gate_w: u8,
}

#[allow(clippy::too_many_arguments)]
fn resolve_hit(
    pat: &BassPattern,
    pat_lo: &BassPattern,
    pat_hi: &BassPattern,
    g_frac: u8,
    step: u32,
    bar: u32,
    genre: usize,
    density: u16,
    feel: u16,
    groove_max_pct: i32,
    voice: &VoiceProfile,
    voice_idx: usize,
    root_midi: u8,
    key: Key,
    die: &Die,
) -> Option<ResolvedHit> {
    let phrase_bar = bar % PHRASE_BARS;
    let rot = phrase_rot(phrase_bar);
    let hits = rot16(pat.hits, rot);
    let hits_fill = rot16(pat.hits_fill, rot);
    let accent_mask = rot16(pat.accent_mask, rot);
    let si = phrase_degree_si(step, phrase_bar);
    let si_step = step % STEPS_PER_BAR;

    let gfeel = groove_feel(feel, groove_max_pct);
    let groove_t = feel_curve(gfeel); // 0..=4095 curved

    let core = bit_set(hits, si_step);
    let ghost = fill_reveal(hits_fill, density, si_step);

    // Groovyland pickups: quiet lead-ins on the 'e'/'a' before the downbeat when
    // Feel is up and DNA left the slot empty (hash keeps sustain lookahead stable;
    // live Die decides whether the pickup actually fires).
    let mut pickup = false;
    if !core && ghost.is_none() && phrase_bar != 7 {
        let pickup_chance = if matches!(si_step, 14 | 15) {
            12u16 + (groove_t / 80)
        } else if si_step % 4 == 3 && groove_t > 2200 {
            6u16 + (groove_t / 120)
        } else {
            0
        };
        if pickup_chance > 0
            && chance_roll(die, step, voice_idx, 41, groove_t) < pickup_chance.min(55) as u8
        {
            pickup = true;
        }
    }

    if !core && ghost.is_none() && !pickup {
        return None;
    }

    let is_accent = bit_set(accent_mask, si_step);

    // Cadence breath on bar 8 only — keep the repeating A bars full.
    if phrase_bar == 7 && core && !is_accent && chance_roll(die, step, voice_idx, 13, groove_t) < 65
    {
        return None;
    }
    if phrase_bar == 7 && !core {
        return None;
    }

    // Voice × genre: pocket players stay sparse in busy breaks; busy voices chill in dub/techno.
    let (ghost_base, approach_base) = voice_genre_bias(voice, genre);
    // Phrase-aware: answer bars invite approaches; breath bar kills chromatic noise.
    let approach_phrase = match phrase_bar {
        4 | 5 => 28u16, // B answer — more walk
        6 => 18,        // A′ ornament
        7 => 0,         // cadence breath
        _ => 8,         // A bars still get light greasy approaches when Feel is up
    };
    let ghost_phrase = match phrase_bar {
        7 => 0u16,
        4 | 5 => (ghost_base as u16 * 3) / 4,
        _ => ghost_base as u16,
    };
    // Feel opens up the Voice's character without replacing it: a voice that
    // plays no ghosts still plays none at full Feel. Syncopation rides along
    // here rather than on Density, so the fader means the same for every voice.
    let feel_scale = 50 + (50 * u32::from(groove_t)) / 4095; // 50..=100
    let ghost_pct = if phrase_bar == 7 {
        0
    } else {
        (((ghost_phrase as u32 * feel_scale) / 100) + u32::from(voice.syncop_bias) / 40).min(100)
            as u16
    };
    let approach_pct = if phrase_bar == 7 {
        0
    } else {
        let scaled = (approach_base as u32 * feel_scale) / 100;
        // Only voices that play approaches at all get the phrase bonus.
        let phrase_add = if approach_base > 0 {
            approach_phrase as u32
        } else {
            0
        };
        (scaled + phrase_add).min(100) as u16
    };

    let fill_frac = ghost.or(if pickup { Some(90u8) } else { None });
    let is_ghost = (!core && fill_frac.is_some()) || pickup;
    if is_ghost && !pickup {
        let frac = fill_frac.unwrap_or(0);
        // Keep more partial ghosts when Feel is cooking (was too eager to drop).
        let drop_bar = if groove_t > 2800 { 90u8 } else { 128u8 };
        if chance_roll(die, step, voice_idx, 3, groove_t) >= ghost_pct.min(100) as u8
            && frac < drop_bar
        {
            return None;
        }
    }

    let mut degree = lerp_i32(
        i32::from(pat_lo.degree[si]),
        i32::from(pat_hi.degree[si]),
        g_frac,
    ) as i8;
    // Pickups: walk chord tones so empty steps aren't all roots.
    if pickup && degree == 0 {
        degree = match chance_roll(die, step, voice_idx, 21, groove_t) % 4 {
            0 => 0,
            1 => 3,
            2 => 4,
            _ => 5,
        };
    }
    // Groovyland neighbor dance on ghosts: ±1 scale degree before pocket fold.
    if is_ghost && groove_t > 1600 && !voice.pocket {
        let wobble = chance_roll(die, step, voice_idx, 23, groove_t);
        if wobble < 35 {
            degree = (i32::from(degree) - 1).rem_euclid(7) as i8;
        } else if wobble > 70 {
            degree = (i32::from(degree) + 1).rem_euclid(7) as i8;
        }
    }
    if voice.pocket {
        degree = fold_pocket(degree);
    }
    // 8-bar harmony: transpose the line by the phrase chord root.
    let chord = i32::from(GENRE_PROG_8[genre.min(NUM_GENRES - 1)][phrase_bar as usize]);
    degree = (i32::from(degree) + chord).rem_euclid(7) as i8;

    // Higher density → allow more adventurous degrees (less folding toward root).
    if density < 900
        && !voice.pocket
        && degree != 0
        && degree != 4
        && chance_roll(die, step, voice_idx, 9, groove_t) > 40
    {
        degree = if degree > 3 { 4 } else { 0 };
    }

    let mut oct = lerp_i32(
        i32::from(pat_lo.oct_off[si]),
        i32::from(pat_hi.oct_off[si]),
        g_frac,
    )
    .clamp(-2, 2) as i8;
    if oct.abs() > voice.oct_span {
        oct = oct.signum() * voice.oct_span;
    }
    // Voices with span≥2: punch accents up/down an octave so leaps are obvious.
    if voice.oct_span >= 2
        && is_accent
        && chance_roll(die, step, voice_idx, 5, groove_t) < 55
    {
        oct = if chance_roll(die, step, voice_idx, 6, groove_t) < 50 {
            voice.oct_span
        } else {
            -voice.oct_span
        };
    }

    let offsets = scale_offsets(key);
    let n = offsets.len().max(1);
    let deg_i = degree.rem_euclid(n as i8) as usize;
    let mut semis = i16::from(offsets[deg_i % n]);
    // Octave wrap for degrees beyond one octave of the mode
    if degree >= n as i8 {
        semis += 12;
    }

    let mut note = (i16::from(root_midi) + semis + i16::from(oct) * 12).clamp(0, 127) as u8;

    // Approaches: chromatic below, or scale-neighbor (Voice × Groove × Die).
    // An approach is a passing note. On a core hit it would replace the target
    // harmony instead of announcing it.
    if is_ghost
        && chance_roll(die, step, voice_idx, 11, groove_t) < approach_pct.min(100) as u8
    {
        let flavor = chance_roll(die, step, voice_idx, 29, groove_t);
        if flavor < 55 || voice.pocket {
            // Classic chromatic approach from below.
            note = note.saturating_sub(1);
        } else if flavor < 80 {
            // Scale degree below target.
            let below = (deg_i + n - 1) % n;
            let below_semi = i16::from(offsets[below]);
            let wrap = if below > deg_i { -12i16 } else { 0 };
            note = (i16::from(root_midi) + below_semi + wrap + i16::from(oct) * 12).clamp(0, 127)
                as u8;
        } else {
            // Upper neighbor (greasy anticipation).
            note = (note as u16 + 1).min(127) as u8;
        }
    }

    let base = lerp_u8(pat_lo.base_vel, pat_hi.base_vel, g_frac);
    let accent = lerp_u8(pat_lo.accent_vel, pat_hi.accent_vel, g_frac)
        .saturating_add(voice.accent_boost)
        .min(127);
    // High groove widens quiet↔loud spread + live rake on accents.
    let quiet_flat = feel_lerp_u16(FLAT_VEL, 45, gfeel);
    let mut character = if is_accent {
        u16::from(accent)
    } else {
        u16::from(base)
    };
    if is_accent && groove_t > 2000 {
        let rake = i32::from(chance_roll(die, step, voice_idx, 31, groove_t)) - 50;
        character = (i32::from(character) + rake / 3).clamp(40, 127) as u16;
    }
    let vel_pct = if is_ghost {
        let g = ghost_vel_pct(fill_frac.unwrap_or(255), 12, if pickup { 38 } else { 45 });
        feel_lerp_u16(quiet_flat, g, gfeel)
    } else {
        feel_lerp_u16(quiet_flat, character, gfeel)
    };

    let mut gate_w = lerp_u8(pat_lo.gate_w[si], pat_hi.gate_w[si], g_frac);
    // Dead notes: some ghosts become short muted chucks when Feel is up.
    if is_ghost
        && groove_t > 1800
        && chance_roll(die, step, voice_idx, 37, groove_t) < (18 + groove_t / 200) as u8
    {
        gate_w = gate_w.min(28);
    }
    // Low groove → even gates; high groove → Voice staccato character.
    let stacc = feel_lerp_u16(100, u16::from(voice.staccato), gfeel) as u8;
    let gate_w = ((u16::from(gate_w) * u16::from(stacc)) / 100).min(100) as u8;

    Some(ResolvedHit {
        note,
        vel_pct,
        gate_w,
    })
}

/// Register arc for solos. A constant +12 reads as "wrong octave", not as a solo:
/// a bass solo pushes off a low anchor, climbs through the phrase, and comes back
/// down to land. Peak height follows the Voice's octave span.
fn solo_register_lift(voice: &VoiceProfile, mask: u16, step: u32, bar: u32, die: &Die) -> i8 {
    let (idx, total) = fill_hit_index(mask, step % STEPS_PER_BAR);
    if total == 0 {
        return 0;
    }
    // Anchor the phrase start low so the climb has a floor to push against.
    if idx == 0 {
        return 0;
    }
    let denom = total.saturating_sub(1).max(1);
    let t = (idx * 255) / denom;
    // Rise to a peak around 60 % of the bar, then fall back for the landing.
    let arc = if t <= 153 {
        (t * 255) / 153
    } else {
        ((255 - t) * 255) / 102
    };
    let ceiling: i8 = if voice.oct_span >= 2 { 2 } else { 1 };
    let mut lift = ((arc * ceiling as usize) / 255) as i8;
    // Answer bars sit lower — call and response instead of one plateau.
    if bar % 2 == 1 && lift > 0 && (die.roll() % 100) < 45 {
        lift -= 1;
    }
    lift.clamp(0, ceiling)
}

fn pitch_class_in_scale(semi: u8, key: Key) -> bool {
    scale_offsets(key).contains(&(semi % 12))
}

/// Next scale tone above/below `main_note` (walks by semitone); chromatic fallback.
fn scale_neighbor_note(main_note: u8, root_midi: u8, key: Key, above: bool) -> u8 {
    let step: i16 = if above { 1 } else { -1 };
    let mut n = i16::from(main_note);
    for _ in 0..12 {
        n += step;
        if !(0..=127).contains(&n) {
            break;
        }
        let semi = (n - i16::from(root_midi)).rem_euclid(12) as u8;
        if pitch_class_in_scale(semi, key) {
            return n as u8;
        }
    }
    if above {
        main_note.saturating_add(1).min(127)
    } else {
        main_note.saturating_sub(1).max(1)
    }
}

/// Monophonic sub-hit pitch — scale neighbors resolved from `cur_key` / root.
#[allow(clippy::too_many_arguments)]
fn bass_ornament_sub_note(
    main_note: u8,
    root_midi: u8,
    key: Key,
    hit: ornament::OrnamentHit,
    hit_idx: usize,
    plan: &ornament::OrnamentPlan,
    pocket: bool,
    roll: u8,
) -> (u8, bool) {
    let (note, dead) =
        ornament::bass_ornament_pitch(main_note, hit, hit_idx, plan, pocket, roll);
    if hit_idx == ornament::main_hit_index(plan) {
        return (note, dead);
    }
    // Replace generic +1 with a true scale-neighbor when the roll asks for it.
    if roll % 5 == 2 && note == main_note.saturating_add(1).min(127) {
        return (scale_neighbor_note(main_note, root_midi, key, true), dead);
    }
    if roll % 5 == 4 && note == main_note.saturating_sub(2).max(1) {
        return (scale_neighbor_note(main_note, root_midi, key, false), dead);
    }
    (note, dead)
}

/// Fold into the playable bass window instead of clamping — a wall turns every
/// out-of-range note into the same repeated pitch.
fn fold_into_range(note: u8, lo: u8, hi: u8) -> u8 {
    let mut n = i16::from(note);
    while n > i16::from(hi) {
        n -= 12;
    }
    while n < i16::from(lo) {
        n += 12;
    }
    n.clamp(i16::from(lo), i16::from(hi)) as u8
}

/// Encode / decode FillShape for globals (no_std, no Enum as Atomic).
fn shape_to_u8(s: FillShape) -> u8 {
    match s {
        FillShape::WalkUp => 0,
        FillShape::WalkDown => 1,
        FillShape::ArpClimb => 2,
        FillShape::OctavePush => 3,
        FillShape::Pedal => 4,
    }
}

fn shape_from_u8(v: u8) -> FillShape {
    match v {
        1 => FillShape::WalkDown,
        2 => FillShape::ArpClimb,
        3 => FillShape::OctavePush,
        4 => FillShape::Pedal,
        _ => FillShape::WalkUp,
    }
}

/// Fill/solo/break path — bypasses density reveal, pickups, and the bar-8 breath cull
/// (fills belong on the turnaround). Melody walks toward the *next* bar's chord.
#[allow(clippy::too_many_arguments)]
fn resolve_fill_hit(
    mask: u16,
    shape: FillShape,
    step: u32,
    bar: u32,
    genre: usize,
    is_solo: bool,
    is_break: bool,
    feel: u16,
    voice: &VoiceProfile,
    root_midi: u8,
    key: Key,
    die: &Die,
    base_vel: u8,
    accent_vel: u8,
) -> Option<ResolvedHit> {
    let si = step % STEPS_PER_BAR;

    if is_break {
        // Pedal: one held root on the downbeat only.
        if si != 0 {
            return None;
        }
    } else if !bit_set(mask, si) {
        return None;
    }

    // Solo onset cap (~12 notes/bar): drop excess hits deterministically.
    if is_solo && !is_break {
        let total = mask.count_ones();
        if total > 12 {
            let (idx, _) = fill_hit_index(mask, si);
            // Keep first 12 in phrase order.
            if idx >= 12 {
                return None;
            }
        }
        // Breath: silence the last 1–2 mask hits on odd bars for phrasing.
        if is_last_fill_hit(mask, si) && (bar % 2 == 1) {
            return None;
        }
        let (idx, total) = fill_hit_index(mask, si);
        if total > 2 && idx + 1 == total.saturating_sub(1) && (bar % 3 == 2) {
            return None;
        }
    }

    let phrase_bar = bar % PHRASE_BARS;
    let next_bar = (phrase_bar + 1) % PHRASE_BARS;
    let prog = &GENRE_PROG_8[genre.min(NUM_GENRES - 1)];
    let cur_chord = prog[phrase_bar as usize] as i8;
    let next_chord = prog[next_bar as usize] as i8;

    let (degree_rel, mut oct) = if is_break {
        (cur_chord, 0i8)
    } else {
        fill_degree(shape, mask, si, cur_chord, next_chord)
    };

    if is_solo {
        oct = oct.saturating_add(solo_register_lift(voice, mask, si, bar, die));
    }
    oct = oct.clamp(-2, 2);

    let offsets = scale_offsets(key);
    let n = offsets.len().max(1);
    let deg_i = degree_rel.rem_euclid(n as i8) as usize;
    let semis = i16::from(offsets[deg_i % n]);
    let raw = (i16::from(root_midi) + semis + i16::from(oct) * 12).clamp(0, 127) as u8;
    let lo = root_midi.saturating_sub(12).max(24);
    let hi = (root_midi as u16 + 24).min(84) as u8;
    let mut note = fold_into_range(raw, lo, hi);

    // Approach into non-final fill hits for greasy voices (skip on pedal/break).
    if !is_break
        && shape != FillShape::Pedal
        && !is_last_fill_hit(mask, si)
        && voice.approach_pct > 40
        && (die.roll() % 100) < (voice.approach_pct / 2) as u16
    {
        note = note.saturating_sub(1).max(lo);
    }

    let vel_pct = if is_solo {
        solo_vel_pct(base_vel, accent_vel, die, feel)
    } else if is_break {
        // Quiet pedal under the drums.
        u16::from(base_vel).saturating_sub(20).max(35)
    } else {
        fill_vel_pct(base_vel, accent_vel, si)
    };

    // Shorter gates for solos / staccato voices so runs articulate.
    let gate_base = if is_solo {
        45u8
    } else if is_break {
        100u8
    } else {
        70u8
    };
    let gate_w = ((u16::from(gate_base) * u16::from(voice.staccato)) / 100)
        .clamp(25, 100) as u8;

    Some(ResolvedHit {
        note,
        vel_pct,
        gate_w,
    })
}

#[embassy_executor::task(pool_size = 4)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(
        app.app_id,
        app.layout_id,
        Params {
            root: MidiNote::from(36), // C2
            scale: 5,                 // Aeolian
            genre: 2,                 // House
            voice: 1,                 // Jamerson (historical index)
            midi_channel: MidiChannel::default(),
            midi_out: MidiOut([true, false, false]), // USB only — all-ports floods cable
            groove_max_pct: 80,
            gatel: 100,
            jack: JACK_OUT,
            range: Range::_Neg5_5V,
            vpo: VoltPerOct::Standard,
            cv_att: 100,
            filter_cc: MidiCc::from(0u8), // off until set
            follow_tonic: true,
            follow_scale: false, // Scale stays the patch's own choice
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
        midi_channel,
        root,
        scale,
        follow_tonic,
        follow_scale,
        genre,
        voice_param,
        groove_max_pct,
        gatel,
        jack_param,
        range,
        vpo,
        cv_att,
        filter_cc,
    ) = params.query(|p| {
        (
            p.midi_out,
            p.midi_channel,
        p.root,
        p.scale.min(SCALE_NAMES.len() - 1),
        p.follow_tonic,
        p.follow_scale,
            p.genre.min(NUM_GENRES - 1),
            p.voice.min(NUM_VOICES - 1),
            p.groove_max_pct.clamp(-100, 100),
            p.gatel.clamp(1, 100),
            p.jack.min(JACK_COUNT - 1),
            p.range,
            p.vpo,
            att_from_pct(p.cv_att),
            p.filter_cc,
        )
    });
    let filter_cc_on = filter_cc.as_u16() != 0;

    // Ticker only — never CLOCK_PUBSUB (Grooves+Vamp+Bassment+Contura combo).
    let ticks = app.clock_ticker();
    let faders = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
    let die = app.use_die();
    let midi = app.use_midi_output(midi_out, midi_channel, false);
    let out_jack = if jack_param == JACK_OUT {
        Some(app.make_out_jack(0, range).await)
    } else {
        None
    };
    let in_jack = if jack_param != JACK_OUT {
        Some(app.make_in_jack(0, Range::_Neg5_5V).await)
    } else {
        None
    };
    if let Some(ref jack) = out_jack {
        jack.set_value(0);
    }

    let (feel, density, muted, stored_voice) =
        storage.query(|s| (s.feel, s.density, s.muted, s.voice));
    // Prefer live scene Voice; fall back to Configurator param when unset/default race.
    let initial_voice = if stored_voice as usize > 0 || voice_param == 0 {
        (stored_voice as usize).min(NUM_VOICES - 1)
    } else {
        voice_param
    };

    let glob_feel = app.make_global(feel);
    let glob_groove_max = app.make_global(groove_max_pct);
    let glob_density = app.make_global(density);
    // Reverse swing rides the sign of Groove max % — scenes never own it.
    let glob_reversed = app.make_global(groove_max_pct < 0);
    let glob_genre = app.make_global(genre);
    let glob_genre_fader = app.make_global(genre_fader_center(genre, NUM_GENRES));
    let glob_muted = app.make_global(muted);
    let glob_voice = app.make_global(initial_voice);
    let glob_reset = app.make_global(false);
    let glob_cv_val = app.make_global(2047u16);
    let long_press_fired = app.make_global(false);
    // Shift at ButtonDown — kept until release so Shift+Long still counts if
    // Shift is released a frame before the long-press event arrives.
    let glob_shift_chord = app.make_global(false);
    let glob_fader_moved = app.make_global(false);
    let glob_fader_at_down = app.make_global(0u16);
    let glob_genre_dirty = app.make_global(false);
    let glob_voice_dirty = app.make_global(false);
    let glob_latch_layer = app.make_global(LatchLayer::Main);
    let glob_voice_flash = app.make_global(0u16);
    let glob_button_duck = app.make_global(0u16);
    // ManagedStorage: only genre_persist writes FRAM (avoids RefCell vs saver_task).
    let glob_storage_dirty = app.make_global(false);
    // Fill/break/solo gesture: Shift tap = fill (or pedal break at high density)
    // until the next downbeat. Shift hold escalates to a register-lifted solo.
    let glob_fill_armed = app.make_global(false);
    let glob_fill_held = app.make_global(false);
    let glob_fill_start = app.make_global(false);
    let glob_fill_variant = app.make_global(0usize);
    let glob_fill_break = app.make_global(false);
    let glob_fill_solo = app.make_global(false);
    let glob_fill_shape = app.make_global(0u8);

    let pending_silence = app.make_global(false);
    let pending_note_off = app.make_global(false);
    let pending_note_on = app.make_global(false);
    let pending_note = app.make_global(0u8);
    let pending_vel = app.make_global(0u16);
    let pending_cc = app.make_global(false);
    let pending_cc_val = app.make_global(0u16);

    let local_key = scale_index_to_key(scale);
    let (root_midi, key) = follow_key::root_and_key(follow_tonic, follow_scale, root, local_key);
    midi.send_note_off(MidiNote::from(root_midi)).await;

    if muted {
        leds.unset(0, Led::Button);
        leds.unset(0, Led::Top);
        leds.unset(0, Led::Bottom);
    } else {
        let color = spectrum_color(glob_genre_fader.get());
        leds.set(0, Led::Button, color, LED_BRIGHTNESS);
        leds.set(0, Led::Top, color, LED_BRIGHTNESS);
        leds.unset(0, Led::Bottom);
    }

    let fut_clock = async {
        let mut origin: u32 = 0;
        let mut origin_set = false;
        let mut note_on = false;
        let mut gate_off_at: Option<u32> = None;
        let mut strike_slot = u32::MAX;
        let mut strike_plan = ornament::OrnamentPlan::empty();
        let mut strike_dues = [u32::MAX; MAX_HITS];
        let mut strike_fired = [false; MAX_HITS];
        let mut strike_hit: Option<ResolvedHit> = None;
        let mut strike_sust = 0u32;
        let mut strike_fill_ctx = (false, false, false); // fill_armed, is_solo, is_break
        // Following the device Tonic turns the global fader into a live
        // transpose. Resolving copies GlobalConfig, so do it once per bar —
        // which also lands a new key on a bar line instead of mid-phrase.
        let mut cur_root = root_midi;
        let mut cur_key = key;
        let mut last_key_bar = u32::MAX;
        let mut last_swing_bar = u32::MAX;
        let mut global_swing_neutral = true;
        // Slot the current fill/break gesture started on, so the release resolves
        // on the *next* downbeat rather than the one it may have started on.
        let mut fill_start_slot = 0u32;

        let mut last_tick = ticks();
        let mut stall_ms = 0u16;

        loop {
            app.delay_millis(1).await;
            let t = ticks();
            let mut do_stop = false;
            if t == last_tick {
                stall_ms = stall_ms.saturating_add(1);
                if stall_ms == 250 {
                    do_stop = true;
                } else {
                    continue;
                }
            } else if t < last_tick {
                do_stop = true;
                last_tick = t;
                stall_ms = 0;
            } else {
                stall_ms = 0;
                last_tick = t;
            }

            if do_stop {
                pending_note_on.set(false);
                pending_note_off.set(false);
                pending_silence.set(true);
                note_on = false;
                if let Some(ref jack) = out_jack {
                    jack.set_value(0);
                }
                gate_off_at = None;
                origin_set = false;
                strike_slot = u32::MAX;
                strike_plan = ornament::OrnamentPlan::empty();
                strike_fired = [false; MAX_HITS];
                strike_hit = None;
                last_swing_bar = u32::MAX;
                glob_reset.set(false);
                glob_fill_armed.set(false);
                glob_fill_start.set(false);
                glob_fill_solo.set(false);
                fill_start_slot = 0;
                if glob_muted.get() {
                    leds.unset(0, Led::Top);
                    leds.unset(0, Led::Bottom);
                } else {
                    leds.set(
                        0,
                        Led::Top,
                        spectrum_color(glob_genre_fader.get()),
                        LED_BRIGHTNESS,
                    );
                    leds.unset(0, Led::Bottom);
                }
                continue;
            }

            let clkn = ticks() as u32;

            if !origin_set || glob_reset.get() {
                origin = clkn;
                origin_set = true;
                strike_slot = u32::MAX;
                strike_plan = ornament::OrnamentPlan::empty();
                strike_fired = [false; MAX_HITS];
                strike_hit = None;
                last_swing_bar = u32::MAX;
                glob_reset.set(false);
                glob_fill_armed.set(false);
                glob_fill_start.set(false);
                glob_fill_solo.set(false);
                fill_start_slot = 0;
            }

            let pos = clkn.wrapping_sub(origin);
            let slot = pos / SIXTEENTH;
            let step = (pos / SIXTEENTH) % STEPS_PER_BAR;
            let bar = slot / STEPS_PER_BAR;
            if bar != last_swing_bar {
                last_swing_bar = bar;
                global_swing_neutral =
                    get_global_config().clock.swing_amount == 0;
            }
            if (follow_tonic || follow_scale) && bar != last_key_bar {
                last_key_bar = bar;
                (cur_root, cur_key) = follow_key::root_and_key(
                    follow_tonic,
                    follow_scale,
                    root,
                    local_key,
                );
            }
            let phase = pos % SIXTEENTH;
            let feel_val = if jack_param == JACK_IN_FEEL {
                mod_u16(glob_feel.get(), glob_cv_val.get())
            } else {
                glob_feel.get()
            };
            let (g_lo, g_hi, g_frac) = genre_pair(glob_genre_fader.get(), NUM_GENRES);
            let near = genre_nearest(glob_genre_fader.get(), NUM_GENRES);
            let pat = &PATTERNS[near];
            let pat_lo = &PATTERNS[g_lo];
            let pat_hi = &PATTERNS[g_hi];
            let voice_idx = glob_voice.get().min(NUM_VOICES - 1);
            let voice = &VOICES[voice_idx];
            let bias = lerp_u8(swing_bias(g_lo), swing_bias(g_hi), g_frac);
            let gmax = glob_groove_max.get();
            let gfeel = groove_feel(feel_val, gmax);
            let swing_pct = feel_swing_pct(bias, feel_val, gmax);
            let timing_char = lerp_i32(
                i32::from(pat_lo.timing[(step % STEPS_PER_BAR) as usize]),
                i32::from(pat_hi.timing[(step % STEPS_PER_BAR) as usize]),
                g_frac,
            );
            // The DNA was written in the ms domain but this is ticks
            // (~21 ms each), so keep it tight — otherwise the bass
            // lands on the next step instead of in the pocket.
            let timing_off = (feel_lerp_i32(0, timing_char, gfeel)
                + groove_timing_boost(feel_val, gmax, step))
                .clamp(-2, 2);
            // The device clock already swings its ticker for internal
            // and external sources. Keep the app's genre swing only
            // while global swing is neutral, otherwise both delays stack.
            let app_swing = if global_swing_neutral {
                swing_delay_ticks(step, swing_pct, glob_reversed.get())
            } else {
                0
            };
            let delay = ((app_swing as i32) + timing_off)
                .clamp(0, (SIXTEENTH as i32) - 1) as u32;

            let density = if jack_param == JACK_IN_DENSITY {
                mod_u16(glob_density.get(), glob_cv_val.get())
            } else {
                glob_density.get()
            };

            // Fill/break/solo gesture. Figure and shape lock in at press so a
            // tap stays one phrase. Holding across a bar line escalates to
            // a register-lifted solo that re-rolls each bar.
            if glob_fill_start.get() {
                glob_fill_start.set(false);
                let v = fill_variant(&die, near, feel_val);
                glob_fill_variant.set(v);
                let break_thresh = if voice.pocket {
                    BREAK_DENSITY_POCKET
                } else {
                    BREAK_DENSITY
                };
                glob_fill_break.set(density >= break_thresh);
                glob_fill_solo.set(false);
                let shape = if glob_fill_break.get() {
                    FillShape::Pedal
                } else {
                    fill_shape(voice, near, &die, feel_val)
                };
                glob_fill_shape.set(shape_to_u8(shape));
                glob_fill_armed.set(true);
                fill_start_slot = slot;
            } else if glob_fill_armed.get() && step == 0 && slot > fill_start_slot {
                if glob_fill_held.get() {
                    glob_fill_solo.set(true);
                    glob_fill_break.set(false);
                    let v = fill_variant(&die, near, feel_val);
                    glob_fill_variant.set(v);
                    let shape = fill_shape(voice, near, &die, feel_val);
                    glob_fill_shape.set(shape_to_u8(shape));
                    fill_start_slot = slot;
                } else {
                    glob_fill_armed.set(false);
                    glob_fill_solo.set(false);
                }
            }

            if let Some(off_at) = gate_off_at {
                if clkn >= off_at {
                    if note_on {
                        pending_note_on.set(false);
                        pending_note_off.set(true);
                        note_on = false;
                    }
                    if let Some(ref jack) = out_jack {
                        jack.set_value(0);
                    }
                    gate_off_at = None;
                    leds.set(
                        0,
                        Led::Bottom,
                        spectrum_color(glob_genre_fader.get()),
                        Brightness::Off,
                    );
                }
            }

            if slot != strike_slot && !glob_muted.get() {
                let is_solo = glob_fill_solo.get();
                let is_break = glob_fill_break.get() && !is_solo;
                let fill_armed = glob_fill_armed.get();
                let v = glob_fill_variant.get().min(FILL_VARIANTS - 1);
                let fill_mask = if fill_armed {
                    if is_solo {
                        SOLO_RHYTHM[near][v]
                    } else if is_break {
                        0b0000_0000_0000_0001 // pedal on downbeat
                    } else {
                        FILL_RHYTHM[near][v]
                    }
                } else {
                    0
                };
                let shape = shape_from_u8(glob_fill_shape.get());

                let hit = if fill_armed {
                    let base = lerp_u8(pat_lo.base_vel, pat_hi.base_vel, g_frac);
                    let accent = lerp_u8(pat_lo.accent_vel, pat_hi.accent_vel, g_frac)
                        .saturating_add(voice.accent_boost)
                        .min(127);
                    resolve_fill_hit(
                        fill_mask,
                        shape,
                        step,
                        bar,
                        near,
                        is_solo,
                        is_break,
                        feel_val,
                        voice,
                        cur_root,
                        cur_key,
                        &die,
                        base,
                        accent,
                    )
                } else {
                    resolve_hit(
                        pat,
                        pat_lo,
                        pat_hi,
                        g_frac,
                        step,
                        bar,
                        near,
                        density,
                        feel_val,
                        gmax,
                        voice,
                        voice_idx,
                        cur_root,
                        cur_key,
                        &die,
                    )
                };

                strike_slot = slot;
                strike_fired = [false; MAX_HITS];
                strike_hit = hit;

                if let Some(base_hit) = hit {
                    let rot = phrase_rot(bar % PHRASE_BARS);
                    let core = if fill_armed {
                        bit_set(fill_mask, step)
                    } else {
                        bit_set(rot16(pat.hits, rot), step)
                    };
                    let any_ghost = if fill_armed {
                        false
                    } else {
                        fill_reveal(rot16(pat.hits_fill, rot), density, step).is_some()
                    };
                    let ghost_extra = ghost_drag_ticks(density, feel_val, gmax);
                    let anchor = if core || !any_ghost {
                        delay
                    } else {
                        (delay + ghost_extra).min(SIXTEENTH - 1)
                    };
                    let artic_ctx = if is_solo {
                        ArticContext::Solo {
                            tier: 2,
                            phrase_bar: bar % 4,
                            feel: feel_val,
                        }
                    } else if fill_armed {
                        ArticContext::Fill {
                            step,
                            feel: feel_val,
                        }
                    } else {
                        ArticContext::Groove {
                            density,
                            feel: feel_val,
                        }
                    };
                    let gv = if voice.pocket {
                        GrooveVoice::Kick
                    } else {
                        GrooveVoice::Snare
                    };
                    let rate_scale = ornament::bass_voice_ornament_scale(
                        voice.ghost_pct,
                        voice.syncop_bias,
                        voice.pocket,
                    );
                    let chance = step_chance(step, voice_idx, 0xB3);
                    strike_plan = if is_break {
                        ornament::OrnamentPlan::single_main()
                    } else {
                        ornament::groove_plan(
                            near,
                            gv,
                            artic_ctx,
                            chance,
                            voice_idx as u32,
                            rate_scale,
                        )
                    };
                    strike_dues =
                        ornament::hit_due_ticks(anchor, &strike_plan, SIXTEENTH);
                    strike_sust = if fill_armed {
                        sustain_fill_sixteenths(
                            fill_mask,
                            step,
                            voice.staccato,
                            is_break,
                        )
                    } else {
                        sustain_sixteenths(
                            rot16(pat.hits, rot),
                            rot16(pat.hits_fill, rot),
                            step,
                            density,
                        )
                    };
                    let _ = base_hit; // sustain context only
                } else {
                    strike_plan = ornament::OrnamentPlan::empty();
                    strike_dues = [u32::MAX; MAX_HITS];
                }
                strike_fill_ctx = (fill_armed, is_solo, is_break);
            }

            if slot == strike_slot {
                if let Some(base_hit) = strike_hit {
                    let (fill_armed, is_solo, is_break) = strike_fill_ctx;
                    let main_i = ornament::main_hit_index(&strike_plan);
                    for hi in 0..strike_plan.len as usize {
                        if strike_fired[hi] || strike_dues[hi] == u32::MAX {
                            continue;
                        }
                        if phase < strike_dues[hi] {
                            continue;
                        }
                        strike_fired[hi] = true;

                        let oh = strike_plan.hits[hi];
                        let roll = step_chance(step, voice_idx, hi as u32 + 0xC1);
                        let (mut note, dead) = bass_ornament_sub_note(
                            base_hit.note,
                            cur_root,
                            cur_key,
                            oh,
                            hi,
                            &strike_plan,
                            voice.pocket,
                            roll,
                        );
                        note = fold_into_range(
                            note,
                            cur_root.saturating_sub(24),
                            cur_root.saturating_add(24).min(127),
                        );
                        let vel_pct = ornament::scale_vel(base_hit.vel_pct, oh);
                        let mut gate_w = oh.gate_pct;
                        if dead {
                            gate_w = gate_w.min(28);
                        }

                        if note_on {
                            pending_note_on.set(false);
                            pending_note_off.set(true);
                        }

                        pending_note.set(note);
                        pending_vel.set(midi_vel(vel_pct));
                        pending_note_on.set(true);
                        note_on = true;

                        if filter_cc_on && hi == main_i {
                            let is_fill = fill_armed && !is_solo && !is_break;
                            pending_cc_val.set(filter_cutoff_12bit(
                                g_lo,
                                g_hi,
                                g_frac,
                                voice,
                                vel_pct,
                                lerp_u8(pat_lo.base_vel, pat_hi.base_vel, g_frac),
                                is_solo,
                                is_fill,
                                is_break,
                                feel_val,
                            ));
                            pending_cc.set(true);
                        }

                        if let Some(ref jack) = out_jack {
                            let counts = note_to_pitch(note).as_counts(range, vpo);
                            jack.set_value(counts);
                        }

                        let max_ticks = (SIXTEENTH * 8).saturating_sub(1);
                        let step_gate = ((SIXTEENTH as i32
                            * strike_sust as i32
                            * gatel
                            * i32::from(gate_w)
                            * i32::from(base_hit.gate_w))
                            / 1_000_000)
                            .clamp(2, max_ticks as i32)
                            as u32;
                        let pulse = step_gate.saturating_sub(1).max(2);
                        gate_off_at = Some(clkn.wrapping_add(pulse));
                        leds.set(
                            0,
                            Led::Bottom,
                            spectrum_color(glob_genre_fader.get()),
                            Brightness::High,
                        );
                        glob_button_duck.set(BUTTON_DUCK_MS);
                        // One sub-hit per poll — avoids MIDI overwrite.
                        break;
                    }
                }
            }

            if glob_latch_layer.get() == LatchLayer::Main {
                leds.set(
                    0,
                    Led::Top,
                    spectrum_color(glob_genre_fader.get()),
                    Brightness::Custom(((step * 255) / STEPS_PER_BAR) as u8),
                );
            }
                
        }
    };

    let fut_voice = async {
        let mut sounding: Option<u8> = None;
        loop {
            app.delay_millis(1).await;

            if pending_silence.get() {
                pending_silence.set(false);
                pending_note_off.set(false);
                pending_note_on.set(false);
                pending_cc.set(false);
                if let Some(n) = sounding.take() {
                    midi.try_send_note_off(MidiNote::from(n));
                }
                continue;
            }

            if pending_cc.get() {
                pending_cc.set(false);
                if filter_cc_on && !glob_muted.get() {
                    midi.try_send_cc(filter_cc, pending_cc_val.get());
                }
            }

            if pending_note_off.get() {
                pending_note_off.set(false);
                if let Some(n) = sounding.take() {
                    midi.try_send_note_off(MidiNote::from(n));
                }
            }

            if pending_note_on.get() {
                pending_note_on.set(false);
                if !glob_muted.get() {
                    let n = pending_note.get();
                    if let Some(prev) = sounding {
                        midi.try_send_note_off(MidiNote::from(prev));
                    }
                    midi.try_send_note_on(MidiNote::from(n), pending_vel.get());
                    sounding = Some(n);
                }
            }
        }
    };

    let fut_buttons = async {
        loop {
            let (_, down_shift) = buttons.wait_for_any_down().await;
            let shift_chord = down_shift || buttons.is_shift_pressed();
            glob_shift_chord.set(shift_chord);
            long_press_fired.set(false);
            if shift_chord {
                // Shift+tap: bass fill (or pedal break) until the next downbeat.
                // Shift+hold across a bar: escalates to a register-lifted solo.
                glob_fill_held.set(true);
                glob_fill_start.set(true);
                buttons.wait_for_up(0).await;
                glob_fill_held.set(false);
                glob_shift_chord.set(false);
            } else {
                glob_fader_moved.set(false);
                glob_fader_at_down.set(faders.get_value());
                buttons.wait_for_up(0).await;
                glob_shift_chord.set(false);
                // Short: mute — same as Contura / Grooves. Reset is CV Dest: Reset.
                if !long_press_fired.get() && !glob_fader_moved.get() {
                    let muted = glob_muted.toggle();
                    glob_storage_dirty.set(true);
                    if muted {
                        leds.unset(0, Led::Button);
                        if let Some(ref jack) = out_jack {
                            jack.set_value(0);
                        }
                        pending_note_on.set(false);
                        pending_note_off.set(false);
                        pending_silence.set(true);
                        glob_fill_armed.set(false);
                        glob_fill_start.set(false);
                        glob_fill_solo.set(false);
                    } else {
                        leds.set(
                            0,
                            Led::Button,
                            spectrum_color(glob_genre_fader.get()),
                            LED_BRIGHTNESS,
                        );
                    }
                }
            }
        }
    };

    let long_press = async {
        loop {
            let (_, is_shift_now) = buttons.wait_for_any_long_press().await;
            long_press_fired.set(true);
            // Prefer chord-at-down: Shift may already be released when the
            // long-press event is delivered (same race Arp avoids by re-polling).
            let shift_chord =
                glob_shift_chord.get() || is_shift_now || buttons.is_shift_pressed();
            if shift_chord {
                // Shift held long enough → escalate fill into solo.
                if glob_fill_armed.get() || glob_fill_start.get() || glob_fill_held.get() {
                    glob_fill_solo.set(true);
                    glob_fill_break.set(false);
                }
                continue;
            }
            // Long: next Voice. Button+fader is the Third-layer Feel scrub.
            if glob_fader_moved.get() {
                continue;
            }
            let cur = glob_voice.get();
            let next = (cur + 1) % NUM_VOICES;
            glob_voice.set(next);
            glob_storage_dirty.set(true);
            glob_voice_dirty.set(true);
            glob_voice_flash.set(VOICE_FLASH_MS);
            if !glob_muted.get() {
                leds.set(0, Led::Button, VOICE_FLASH_COLOR[next], Brightness::High);
            }
        }
    };

    let fut_faders = async {
        let mut latch = app.make_latch(faders.get_value());
        loop {
            faders.wait_for_change_at(0).await;
            let fader_val = faders.get_value();
            let latch_layer = glob_latch_layer.get();

            if buttons.is_button_pressed(0) && !buttons.is_shift_pressed() {
                let delta = fader_val.abs_diff(glob_fader_at_down.get());
                if delta > FADER_MOVE_THRESH {
                    glob_fader_moved.set(true);
                }
            }

            let target_value = match latch_layer {
                LatchLayer::Main => glob_density.get(),
                LatchLayer::Alt => glob_genre_fader.get(),
                LatchLayer::Third => glob_feel.get(),
            };

            if let Some(new_value) = latch.update(fader_val, latch_layer, target_value) {
                match latch_layer {
                    LatchLayer::Main => {
                        glob_density.set(new_value);
                        glob_storage_dirty.set(true);
                    }
                    LatchLayer::Alt => {
                        glob_genre_fader.set(new_value);
                        let g = value_to_index(new_value, NUM_GENRES);
                        if g != glob_genre.get() {
                            glob_genre.set(g);
                            glob_genre_dirty.set(true);
                        }
                    }
                    LatchLayer::Third => {
                        glob_feel.set(new_value);
                        glob_storage_dirty.set(true);
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
                    let (feel, density, muted, voice) =
                        storage.query(|s| (s.feel, s.density, s.muted, s.voice));
                    glob_feel.set(feel);
                    glob_density.set(density);
                    glob_muted.set(muted);
                    glob_voice.set((voice as usize).min(NUM_VOICES - 1));
                    glob_fill_armed.set(false);
                    glob_fill_start.set(false);
                    glob_fill_solo.set(false);
                    let (g, gmax) = params.query(|p| {
                        (
                            p.genre.min(NUM_GENRES - 1),
                            p.groove_max_pct.clamp(-100, 100),
                        )
                    });
                    glob_genre.set(g);
                    glob_genre_fader.set(genre_fader_center(g, NUM_GENRES));
                    glob_groove_max.set(gmax);
                    glob_reversed.set(gmax < 0);
                    if muted {
                        leds.unset(0, Led::Button);
                    } else {
                        leds.set(
                            0,
                            Led::Button,
                            spectrum_color(glob_genre_fader.get()),
                            LED_BRIGHTNESS,
                        );
                    }
                }
                SceneEvent::SaveScene(scene) => storage.save_to_scene(scene).await,
            }
        }
    };

    let shift = async {
        let mut fill_led_prev = false;
        let mut prev_gate_high = false;
        let mut last_seen_ticks: u32 = ticks() as u32;
        let mut stall_ms: u16 = CLOCK_STALL_MS;
        loop {
            app.delay_millis(1).await;

            let now_ticks = ticks() as u32;
            if now_ticks != last_seen_ticks {
                last_seen_ticks = now_ticks;
                stall_ms = 0;
            } else {
                stall_ms = stall_ms.saturating_add(1);
            }
            let clock_alive = stall_ms < CLOCK_STALL_MS;

            if let Some(ref input) = in_jack {
                let in_val = attenuate_bipolar(input.get_value(), cv_att);
                glob_cv_val.set(in_val);
                if jack_param == JACK_IN_RESET {
                    let high = in_val >= TRIG_HIGH;
                    if high && !prev_gate_high {
                        glob_reset.set(true);
                    }
                    prev_gate_high = high;
                } else {
                    prev_gate_high = false;
                }
            }

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

            match latch_active_layer {
                LatchLayer::Alt => {
                    let fader_now = faders.get_value();
                    let color = spectrum_color(fader_now);
                    let led = split_unsigned_value(fader_now);
                    leds.set(0, Led::Top, color, Brightness::Custom(led[0]));
                    leds.set(0, Led::Bottom, color, Brightness::Custom(led[1]));
                    if !glob_fill_armed.get() && glob_voice_flash.get() == 0 {
                        leds.set(0, Led::Button, color, Brightness::High);
                    }
                }
                LatchLayer::Third => {
                    let s = glob_feel.get();
                    let app_c = Color::Cyan;
                    // White → app → Red blend by feel (Grooves Third meter style).
                    let t = s as u32;
                    let (r, g, b) = if t < 2048 {
                        let f = t;
                        let rr = 255u32;
                        let gg = 255u32 - (f * 255 / 2048);
                        let bb = 255u32 - (f * 255 / 2048);
                        // Mix toward cyan (0,255,255) as feel rises through first half.
                        let mix = f;
                        (
                            (rr * (2048 - mix) / 2048) as u8,
                            ((gg * (2048 - mix) + 255 * mix) / 2048) as u8,
                            ((bb * (2048 - mix) + 255 * mix) / 2048) as u8,
                        )
                    } else {
                        let f = t - 2048;
                        // Cyan → Red
                        (
                            ((255 * f) / 2048) as u8,
                            ((255u32 * (2048 - f)) / 2048) as u8,
                            ((255u32 * (2048 - f)) / 2048) as u8,
                        )
                    };
                    let _ = app_c;
                    leds.set(
                        0,
                        Led::Top,
                        Color::Custom(r, g, b),
                        Brightness::Custom((s / 16) as u8),
                    );
                }
                LatchLayer::Main => {
                    if !clock_alive && !glob_muted.get() {
                        leds.set(
                            0,
                            Led::Top,
                            spectrum_color(glob_genre_fader.get()),
                            LED_BRIGHTNESS,
                        );
                        leds.unset(0, Led::Bottom);
                    }
                }
            }

            // Fill/solo/break takes over the button LED for the whole gesture:
            // bright white while filling/soloing, dim while pedaling a break.
            let fill_led = glob_fill_armed.get();
            if fill_led && !glob_muted.get() {
                let bright = if glob_fill_break.get() && !glob_fill_solo.get() {
                    Brightness::Low
                } else {
                    Brightness::High
                };
                leds.set(0, Led::Button, Color::White, bright);
            } else if fill_led_prev {
                if glob_muted.get() {
                    leds.unset(0, Led::Button);
                } else {
                    leds.set(
                        0,
                        Led::Button,
                        spectrum_color(glob_genre_fader.get()),
                        LED_BRIGHTNESS,
                    );
                }
            }
            fill_led_prev = fill_led;

            let flash_left = glob_voice_flash.get();
            if flash_left > 0 {
                let left = flash_left.saturating_sub(1);
                glob_voice_flash.set(left);
                if left == 0 && !fill_led {
                    if glob_muted.get() {
                        leds.unset(0, Led::Button);
                    } else {
                        leds.set(
                            0,
                            Led::Button,
                            spectrum_color(glob_genre_fader.get()),
                            LED_BRIGHTNESS,
                        );
                    }
                } else if left > 0 && !fill_led {
                    let v = glob_voice.get().min(NUM_VOICES - 1);
                    leds.set(0, Led::Button, VOICE_FLASH_COLOR[v], Brightness::High);
                }
            }

            let duck = glob_button_duck.get();
            if duck > 0 {
                glob_button_duck.set(duck.saturating_sub(1));
            }
            if !fill_led
                && flash_left == 0
                && !glob_muted.get()
                && latch_active_layer != LatchLayer::Alt
            {
                let bright = if duck > 0 {
                    Brightness::Low
                } else {
                    LED_BRIGHTNESS
                };
                leds.set(
                    0,
                    Led::Button,
                    spectrum_color(glob_genre_fader.get()),
                    bright,
                );
            }
        }
    };

    let genre_persist = async {
        loop {
            app.delay_millis(40).await;
            if glob_genre_dirty.get() {
                glob_genre_dirty.set(false);
                let g = glob_genre.get().min(NUM_GENRES - 1);
                params.update(|p| p.genre = g).await;
            }
            if glob_voice_dirty.get() {
                glob_voice_dirty.set(false);
                let v = glob_voice.get().min(NUM_VOICES - 1);
                params.update(|p| p.voice = v).await;
            }
            if glob_storage_dirty.get() {
                glob_storage_dirty.set(false);
                storage.modify_and_save(|s| {
                    s.feel = glob_feel.get();
                    s.density = glob_density.get();
                    s.muted = glob_muted.get();
                    s.voice = glob_voice.get() as u8;
                });
            }
        }
    };

    join(
        join5(fut_clock, fut_voice, fut_buttons, fut_faders, scene_handler),
        join3(shift, long_press, genre_persist),
    )
    .await;
}
