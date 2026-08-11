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
use midly::num::u7;
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue,
    latch::LatchLayer,
    quantizer::Pitch,
    utils::{attenuate_bipolar, split_unsigned_value, value_to_index},
    AppIcon, Brightness, Color, Config, Key, MidiChannel, MidiNote, MidiOut, Note,
    Param, Range, Value, VoltPerOct, APP_MAX_PARAMS,
};

use crate::app::{
    App, AppParams, AppStorage, Die, Led, ManagedStorage, ParamStore, SceneEvent,
};
use crate::apps::genre_palette::{genre_fader_center, GENRE_NAMES, GENRE_PROG_8, NUM_GENRES};
use crate::apps::groove::{
    feel_curve, feel_lerp_i32, feel_lerp_u16, swing_bias, swing_delay_ticks, FLAT_VEL, SIXTEENTH,
};
use crate::apps::led_fx::{genre_nearest, genre_pair, lerp_i32, lerp_u8, spectrum_color};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 14;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
const BUTTON_DUCK_MS: u16 = 25;
const FADER_MOVE_THRESH: u16 = 64;
const CLOCK_STALL_MS: u16 = 100;
const VOICE_FLASH_MS: u16 = 300;

const STEPS_PER_BAR: u32 = 16;

const CV_JACK_OUT: usize = 0;
const CV_JACK_IN: usize = 1;

const DEST_DENSITY: usize = 0;
const DEST_FEEL: usize = 1;
const DEST_RESET: usize = 2;
const DEST_COUNT: usize = 3;

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
    /// Extra density reveal bias in 0..=1600 (added before clamp).
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

/// Rotate a 16-step mask left by `n` sixteenths (phrase answer / displacement).
fn rot16(mask: u16, n: u32) -> u16 {
    let n = n % 16;
    if n == 0 {
        mask
    } else {
        mask.rotate_left(n)
    }
}

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
        hits_fill: 0b1110_1110_1110_1110,
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
        hits_fill: 0b1110_1110_1110_1110,
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
        hits_fill: 0b1010_1110_1101_1110,
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
        hits_fill: 0b1111_0110_1101_1010,
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
        hits_fill: 0b0111_0110_1101_1110,
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
.add_param(Param::i32 {
    name: "Groove max %",
    min: 10,
    max: 100,
})
.add_param(Param::i32 {
    name: "GATE %",
    min: 1,
    max: 100,
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
    variants: &["Density", "Feel", "Reset"],
})
.add_param(Param::i32 {
    name: "CV Att",
    min: 0,
    max: 100,
})
.add_param(Param::Enum {
    name: "Swing Dir",
    variants: &["Normal", "Reverse"],
});

pub struct Params {
    root: MidiNote,
    scale: usize,
    genre: usize,
    voice: usize,
    midi_channel: MidiChannel,
    midi_out: MidiOut,
    /// Caps Third Feel: swing + microtiming + ghost drag + velocity contrast.
    groove_max_pct: i32,
    gatel: i32,
    cv_jack: usize,
    range: Range,
    vpo: VoltPerOct,
    cv_dest: usize,
    cv_att: i32,
    /// Swing direction: Normal (offbeats late) vs Reverse (offbeats early).
    swing_dir: usize,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < 13 {
            return None;
        }
        let swing_dir = if values.len() > 13 {
            usize::from_value(values[13]).min(1)
        } else {
            0
        };
        Some(Self {
            root: MidiNote::from_value(values[0]),
            scale: usize::from_value(values[1]).min(SCALE_NAMES.len() - 1),
            genre: usize::from_value(values[2]).min(NUM_GENRES - 1),
            voice: usize::from_value(values[3]).min(NUM_VOICES - 1),
            midi_channel: MidiChannel::from_value(values[4]),
            midi_out: MidiOut::from_value(values[5]),
            groove_max_pct: i32::from_value(values[6]).clamp(10, 100),
            gatel: i32::from_value(values[7]).clamp(1, 100),
            cv_jack: usize::from_value(values[8]).min(1),
            range: Range::from_value(values[9]),
            vpo: VoltPerOct::from_value(values[10]),
            cv_dest: usize::from_value(values[11]).min(DEST_COUNT - 1),
            cv_att: i32::from_value(values[12]).clamp(0, 100),
            swing_dir,
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
        vec.push(self.cv_jack.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.vpo.into()).unwrap();
        vec.push(self.cv_dest.into()).unwrap();
        vec.push(self.cv_att.into()).unwrap();
        vec.push(self.swing_dir.into()).unwrap();
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
    feel: u16,
    density: u16,
    reversed: bool,
    muted: bool,
    voice: u8,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            // Mid-high so Third already grooves out of the box.
            feel: 3400,
            density: 2048,
            reversed: false,
            muted: false,
            voice: 1, // Jamerson
        }
    }
}

impl AppStorage for Storage {}

fn bit_set(mask: u16, step: u32) -> bool {
    mask & (1u16 << (step % STEPS_PER_BAR)) != 0
}

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

/// When density is in the upper third and DNA left the step empty, insert
/// synthetic fill hits so max Density actually feels busy (fills alone are
/// still capped by the pattern mask).
fn synth_reveal(density: u16, step: u32, voice_idx: usize) -> Option<u8> {
    const FLOOR: u16 = 2730;
    if density < FLOOR {
        return None;
    }
    let span = u32::from(4095u16.saturating_sub(FLOOR));
    let t = u32::from(density.saturating_sub(FLOOR)).min(span);
    // Offbeats first; downbeats rarer (cores usually cover them).
    let chance = if step % 2 == 1 {
        (t * 92 / span) as u8
    } else {
        (t * 55 / span) as u8
    };
    if step_chance(step, voice_idx, 17 + (step % 4)) >= chance {
        return None;
    }
    Some(((t * 255) / span).max(80) as u8)
}

fn ghost_vel_pct(frac: u8, quiet: u16, full: u16) -> u16 {
    quiet + ((full - quiet) as u32 * frac as u32 / 255) as u16
}

/// Extra microtiming ticks at high groove (beyond pattern DNA).
fn groove_timing_boost(feel: u16, groove_max_pct: i32, step: u32) -> i32 {
    let g = groove_feel(feel, groove_max_pct);
    let t = i32::from(feel_curve(g));
    // Odd 16ths push later; some even 16ths pull early — classic pocket.
    // Wider throw than before so Feel actually moves the pocket.
    let signed = if step % 2 == 1 { 5 } else { -2 };
    (signed * t) / 4095
}

fn ghost_drag_ticks(density: u16, feel: u16, groove_max_pct: i32) -> u32 {
    let g = groove_feel(feel, groove_max_pct);
    let dens = (density as u32 * 3) / 4095;
    let feel_extra = (u32::from(feel_curve(g)) * 4) / 4095;
    (dens + feel_extra).min(6)
}

/// How many 16ths a hit can sustain before the next sounded step (1..=8).
fn sustain_sixteenths(
    hits: u16,
    hits_fill: u16,
    step: u32,
    density: u16,
    voice_idx: usize,
) -> u32 {
    let mut n = 1u32;
    for look in 1..8u32 {
        let s = step + look;
        if bit_set(hits, s)
            || fill_reveal(hits_fill, density, s).is_some()
            || synth_reveal(density, s, voice_idx).is_some()
        {
            break;
        }
        n += 1;
    }
    n
}

fn midi_vel(mult: u16) -> u16 {
    ((4095u32 * mult as u32) / 100).min(4095) as u16
}

/// Effective Feel after Groove max % ceiling (0..=4095).
fn groove_feel(feel: u16, groove_max_pct: i32) -> u16 {
    let cap = groove_max_pct.clamp(10, 100) as u32;
    ((u32::from(feel) * cap) / 100).min(4095) as u16
}

/// Swing % from genre bias × curved Feel, capped by Groove max %.
fn feel_swing_pct(bias: u8, feel: u16, groove_max_pct: i32) -> i32 {
    let g = groove_feel(feel, groove_max_pct);
    let f = u32::from(feel_curve(g));
    // Bias is a floor-ish character; at full groove allow up to Groove max.
    let from_bias = (u32::from(bias) * f) / 4095;
    let toward_max = (groove_max_pct.clamp(10, 100) as u32 * f) / 4095;
    // Blend: keep genre DNA but let Groove max open the ceiling.
    let pct = (from_bias * 2 + toward_max) / 3;
    pct.min(groove_max_pct.clamp(10, 100) as u32) as i32
}

fn midi_u8(note: MidiNote) -> u8 {
    u7::from(note).as_int()
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

/// Deterministic 0..99 hash from step + voice (hit masks / sustain lookahead).
fn step_chance(step: u32, voice: usize, salt: u32) -> u8 {
    let x = step
        .wrapping_mul(37)
        .wrapping_add(voice as u32)
        .wrapping_mul(17)
        .wrapping_add(salt);
    (x % 100) as u8
}

/// Groove-weighted roll: low Feel stays hash-locked; high Feel leans on live Die.
fn chance_roll(die: &Die, step: u32, voice: usize, salt: u32, groove_t: u16) -> u8 {
    let hashed = u32::from(step_chance(step, voice, salt));
    let live = u32::from(die.roll() % 100);
    // Cap live mix at ~80% so genre DNA still peeks through at max Feel.
    let w = (u32::from(groove_t) * 4) / 5;
    ((hashed * (4095 - w) + live * w) / 4095) as u8
}

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
    scale: usize,
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
    let synth = if !core && ghost.is_none() {
        synth_reveal(density, step, voice_idx)
    } else {
        None
    };

    // Groovyland pickups: quiet lead-ins on the 'e'/'a' before the downbeat when
    // Feel is up and DNA left the slot empty (hash keeps sustain lookahead stable;
    // live Die decides whether the pickup actually fires).
    let mut pickup = false;
    if !core && ghost.is_none() && synth.is_none() && phrase_bar != 7 {
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

    if !core && ghost.is_none() && synth.is_none() && !pickup {
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

    // Air in the pocket: at high Feel, drop soft (non-accent) cores for space —
    // classic groove negative space, stronger for busy voices.
    if core
        && !is_accent
        && !pickup
        && groove_t > 2200
        && phrase_bar != 7
    {
        let air = if voice.pocket {
            ((groove_t - 2200) * 18) / 1895
        } else {
            ((groove_t - 2200) * 32) / 1895
        };
        if chance_roll(die, step, voice_idx, 19, groove_t) < air.min(40) as u8 {
            return None;
        }
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
    // High groove opens Voice ghost/approach character (except cadence breath).
    // Extra groovyland bias: Feel unlocks more ghosts/approaches than before.
    let ghost_pct = if phrase_bar == 7 {
        0
    } else {
        let opened = ghost_phrase + ((100u16.saturating_sub(ghost_phrase)) * groove_t / 4095) * 2 / 3;
        opened.min(100)
    };
    let approach_pct = if phrase_bar == 7 {
        0
    } else {
        (approach_base as u16 + approach_phrase)
            .min(100)
            .saturating_add(((100u16.saturating_sub(approach_base as u16)) * groove_t / 4095) / 2)
            .min(100)
    };

    let fill_frac = ghost.or(synth).or(if pickup { Some(90u8) } else { None });
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
    // Synthetic fills / pickups: walk chord tones so empty steps aren't all roots.
    if (synth.is_some() || pickup) && degree == 0 {
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

    let offsets = scale_offsets(scale_index_to_key(scale));
    let n = offsets.len().max(1);
    let deg_i = degree.rem_euclid(n as i8) as usize;
    let mut semis = i16::from(offsets[deg_i % n]);
    // Octave wrap for degrees beyond one octave of the mode
    if degree >= n as i8 {
        semis += 12;
    }

    let mut note = (i16::from(root_midi) + semis + i16::from(oct) * 12).clamp(0, 127) as u8;

    // Approaches: chromatic below, or scale-neighbor (Voice × Groove × Die).
    if chance_roll(die, step, voice_idx, 11, groove_t) < approach_pct.min(100) as u8
        && (core || pickup || density > 1800 || groove_t > 1600)
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
    scale: usize,
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

    // Solo register: +1 octave (Jaco/Claypool +2 when span allows).
    if is_solo {
        let lift = if voice.oct_span >= 2 { 2 } else { 1 };
        oct = oct.saturating_add(lift);
    }
    if oct.abs() > voice.oct_span.max(if is_solo { 2 } else { 1 }) {
        oct = oct.signum() * voice.oct_span.max(1);
    }

    let offsets = scale_offsets(scale_index_to_key(scale));
    let n = offsets.len().max(1);
    let deg_i = degree_rel.rem_euclid(n as i8) as usize;
    let semis = i16::from(offsets[deg_i % n]);
    let mut note =
        (i16::from(root_midi) + semis + i16::from(oct) * 12).clamp(24, 84) as u8;
    // Also clamp relative to root window.
    let lo = root_midi.saturating_sub(12).max(24);
    let hi = (root_midi as u16 + 24).min(84) as u8;
    note = note.clamp(lo, hi);

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
            cv_jack: CV_JACK_OUT,
            range: Range::_Neg5_5V,
            vpo: VoltPerOct::Standard,
            cv_dest: DEST_DENSITY,
            cv_att: 100,
            swing_dir: 0,
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
        genre,
        voice_param,
        groove_max_pct,
        gatel,
        cv_jack,
        range,
        vpo,
        cv_dest,
        cv_att,
        swing_dir,
    ) = params.query(|p| {
        (
            p.midi_out,
            p.midi_channel,
            p.root,
            p.scale.min(SCALE_NAMES.len() - 1),
            p.genre.min(NUM_GENRES - 1),
            p.voice.min(NUM_VOICES - 1),
            p.groove_max_pct.clamp(10, 100),
            p.gatel.clamp(1, 100),
            p.cv_jack.min(1),
            p.range,
            p.vpo,
            p.cv_dest.min(DEST_COUNT - 1),
            att_from_pct(p.cv_att),
            p.swing_dir == 1,
        )
    });

    // Ticker only — never CLOCK_PUBSUB (Grooves+Vamp+Bassment+Contura combo).
    let ticks = app.clock_ticker();
    let faders = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
    let die = app.use_die();
    let midi = app.use_midi_output(midi_out, midi_channel, false);
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

    let (feel, density, _stored_reversed, muted, stored_voice) =
        storage.query(|s| (s.feel, s.density, s.reversed, s.muted, s.voice));
    // Prefer live scene Voice; fall back to Configurator param when unset/default race.
    let initial_voice = if stored_voice as usize > 0 || voice_param == 0 {
        (stored_voice as usize).min(NUM_VOICES - 1)
    } else {
        voice_param
    };

    let glob_feel = app.make_global(feel);
    let glob_groove_max = app.make_global(groove_max_pct);
    let glob_density = app.make_global(density);
    // Swing Dir is a Config param — scenes no longer own the toggle.
    let glob_reversed = app.make_global(swing_dir);
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

    let root_midi = midi_u8(root);
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
        let mut sounding_note: u8 = 0;
        let mut gate_off_at: Option<u32> = None;
        let mut last_fired_slot = u32::MAX;
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
                    last_fired_slot = u32::MAX;
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
                        last_fired_slot = u32::MAX;
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
                    // Pattern DNA ×2 + pocket boost — all scaled by Groove Feel.
                    let timing_off = feel_lerp_i32(0, timing_char * 2, gfeel)
                        + groove_timing_boost(feel_val, gmax, step);
                    let delay = ((swing_delay_ticks(step, swing_pct, glob_reversed.get()) as i32)
                        + timing_off)
                        .clamp(0, (SIXTEENTH as i32) - 1) as u32;

                    let mut density = if cv_jack == CV_JACK_IN && cv_dest == DEST_DENSITY {
                        mod_u16(glob_density.get(), glob_cv_val.get())
                    } else {
                        glob_density.get()
                    };
                    density = density.saturating_add(voice.syncop_bias).min(4095);

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

                    if slot != last_fired_slot && !glob_muted.get() {
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
                                root_midi,
                                scale,
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
                                root_midi,
                                scale,
                                &die,
                            )
                        };

                        let rot = phrase_rot(bar % PHRASE_BARS);
                        let core = if fill_armed {
                            bit_set(fill_mask, step)
                        } else {
                            bit_set(rot16(pat.hits, rot), step)
                        };
                        let any_ghost = if fill_armed {
                            false
                        } else {
                            fill_reveal(rot16(pat.hits_fill, rot), density, step)
                                .or_else(|| {
                                    if !core {
                                        synth_reveal(density, step, voice_idx)
                                    } else {
                                        None
                                    }
                                })
                                .is_some()
                        };
                        let ghost_extra = ghost_drag_ticks(density, feel_val, gmax);
                        let required_delay = if core || !any_ghost {
                            delay
                        } else {
                            (delay + ghost_extra).min(SIXTEENTH - 1)
                        };

                        if phase >= required_delay {
                            last_fired_slot = slot;
                            if let Some(hit) = hit {
                                let legato = note_on && hit.note == sounding_note;
                                if note_on && !legato {
                                    pending_note_on.set(false);
                                    pending_note_off.set(true);
                                }

                                if !legato {
                                    pending_note.set(hit.note);
                                    pending_vel.set(midi_vel(hit.vel_pct));
                                    pending_note_on.set(true);
                                    sounding_note = hit.note;
                                }
                                note_on = true;

                                if let Some(ref jack) = out_jack {
                                    let counts =
                                        note_to_pitch(hit.note).as_counts(range, vpo);
                                    jack.set_value(counts);
                                }

                                let sust = if fill_armed {
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
                                        voice_idx,
                                    )
                                };
                                let max_ticks = (SIXTEENTH * 8).saturating_sub(1);
                                let step_gate = ((SIXTEENTH as i32
                                    * sust as i32
                                    * gatel
                                    * i32::from(hit.gate_w))
                                    / 10_000)
                                    .clamp(2, max_ticks as i32)
                                    as u32;
                                // Leave a tiny gap before the next attack unless legato.
                                let gap = if legato { 0 } else { 1 };
                                let pulse = step_gate.saturating_sub(gap).max(2);
                                gate_off_at = Some(clkn.wrapping_add(pulse));
                                leds.set(
                                    0,
                                    Led::Bottom,
                                    spectrum_color(glob_genre_fader.get()),
                                    Brightness::High,
                                );
                                glob_button_duck.set(BUTTON_DUCK_MS);
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
                if let Some(n) = sounding.take() {
                    midi.try_send_note_off(MidiNote::from(n));
                }
                continue;
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
                    let (g, sd) = params.query(|p| {
                        (p.genre.min(NUM_GENRES - 1), p.swing_dir == 1)
                    });
                    glob_genre.set(g);
                    glob_genre_fader.set(genre_fader_center(g, NUM_GENRES));
                    glob_reversed.set(sd);
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
                    s.reversed = glob_reversed.get();
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
