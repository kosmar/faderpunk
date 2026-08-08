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
    AppIcon, Brightness, ClockDivision, Color, Config, Key, MidiChannel, MidiNote, MidiOut, Note,
    Param, Range, Value, VoltPerOct, APP_MAX_PARAMS,
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
pub const PARAMS: usize = 13;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
const REVERSE_FADE_MS: u16 = 500;
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
    /// Extra density reveal bias in 0..=512 (added before clamp).
    syncop_bias: u16,
    /// Accent velocity boost 0..=40.
    accent_boost: u8,
    /// Prefer root/fifth: fold odd degrees toward 0 or 4.
    pocket: bool,
}

const VOICES: [VoiceProfile; NUM_VOICES] = [
    // Mingus — chromatic approach, odd accents (bebop / post-bop)
    VoiceProfile {
        oct_span: 1,
        approach_pct: 90,
        ghost_pct: 40,
        staccato: 80,
        syncop_bias: 350,
        accent_boost: 24,
        pocket: false,
    },
    // Jamerson — locked Motown pocket (root/fifth only, no leaps)
    VoiceProfile {
        oct_span: 0,
        approach_pct: 0,
        ghost_pct: 5,
        staccato: 90,
        syncop_bias: 0,
        accent_boost: 4,
        pocket: true,
    },
    // Bootsy — funk ghosts + octave drops
    VoiceProfile {
        oct_span: 1,
        approach_pct: 25,
        ghost_pct: 85,
        staccato: 55,
        syncop_bias: 700,
        accent_boost: 30,
        pocket: false,
    },
    // Jaco — melodic, sustained, approaches + octave leaps
    VoiceProfile {
        oct_span: 2,
        approach_pct: 70,
        ghost_pct: 25,
        staccato: 100,
        syncop_bias: 120,
        accent_boost: 10,
        pocket: false,
    },
    // Robbie — Shakespeare dub pocket: deep, sustained, sparse
    VoiceProfile {
        oct_span: 0,
        approach_pct: 8,
        ghost_pct: 12,
        staccato: 100,
        syncop_bias: 40,
        accent_boost: 6,
        pocket: true,
    },
    // Flabba — Holt roots walk: roomy gates, mild syncop (duo with Robbie)
    VoiceProfile {
        oct_span: 1,
        approach_pct: 18,
        ghost_pct: 30,
        staccato: 95,
        syncop_bias: 180,
        accent_boost: 14,
        pocket: false,
    },
    // Flea — short gates, max syncop, hard accents, octave jumps
    VoiceProfile {
        oct_span: 2,
        approach_pct: 15,
        ghost_pct: 50,
        staccato: 28,
        syncop_bias: 900,
        accent_boost: 40,
        pocket: false,
    },
    // Claypool — quirky leaps, chromatic nudges, odd staccato
    VoiceProfile {
        oct_span: 2,
        approach_pct: 55,
        ghost_pct: 45,
        staccato: 40,
        syncop_bias: 650,
        accent_boost: 35,
        pocket: false,
    },
];

/// Distinct flash colors so Shift+Long Voice cycle is obvious on the button.
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

/// 8-bar chord roots (scale degrees). First 4 ≈ Chord Vamp tropes; bars 5–8 = answer / turnaround.
const PHRASE_PROG: [[u8; 8]; NUM_GENRES] = [
    // Dub — i–IV–i–V | i–IV–V–i
    [0, 3, 0, 4, 0, 3, 4, 0],
    // Disco — I–vi–IV–V | I–IV–V–I
    [0, 5, 3, 4, 0, 3, 4, 0],
    // House — i–VII–VI–VII | i–VI–VII–i
    [0, 6, 5, 6, 0, 5, 6, 0],
    // Techno — pedal + rare V | pedal + drop
    [0, 0, 0, 4, 0, 0, 4, 0],
    // Trip-Hop — i–VII–VI–v | i–VI–v–i
    [0, 6, 5, 4, 0, 5, 4, 0],
    // Hip-Hop — i–VI–III–VII | i–III–VI–VII
    [0, 5, 2, 6, 0, 2, 5, 6],
    // Jungle — i–VII–VI–III | i–VI–III–VII
    [0, 6, 5, 2, 0, 5, 2, 6],
    // UK Garage — i–III–VI–VII | i–VI–III–VII
    [0, 2, 5, 6, 0, 5, 2, 6],
    // Dubstep — i–i–VI–VII | i–VI–VII–i
    [0, 0, 5, 6, 0, 5, 6, 0],
];

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
    // 0 Dub — sparse half-time, root-heavy
    BassPattern {
        hits: 0b0000_0001_0000_0001,
        hits_fill: 0b0000_0100_0001_0000,
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
        hits_fill: 0b0100_0100_0100_0100,
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
        hits_fill: 0b0100_0000_0100_0100,
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
        hits_fill: 0b0000_0100_0000_0000,
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
        hits_fill: 0b0000_1000_0001_0000,
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
        hits_fill: 0b0000_0100_0100_0100,
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
        hits_fill: 0b0010_0000_0100_1000,
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
        hits_fill: 0b0010_0000_0100_0000,
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
        hits_fill: 0b0000_1000_0100_0000,
        accent_mask: 0b0000_0000_0000_0001,
        base_vel: 80,
        accent_vel: 115,
        timing: [0, 1, 0, 2, 1, 2, 0, 2, 0, 1, 0, 2, 1, 2, 0, 2],
        degree: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 6, 0, 0],
        oct_off: [-1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        gate_w: [100, 40, 40, 40, 40, 40, 40, 40, 90, 40, 70, 40, 40, 75, 40, 40],
    },
];

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
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < 13 {
            return None;
        }
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

fn ghost_vel_pct(frac: u8, quiet: u16, full: u16) -> u16 {
    quiet + ((full - quiet) as u32 * frac as u32 / 255) as u16
}

fn ghost_drag_ticks(density: u16, feel: u16, groove_max_pct: i32) -> u32 {
    let g = groove_feel(feel, groove_max_pct);
    let dens = (density as u32 * 3) / 4095;
    let feel_extra = (u32::from(feel_curve(g)) * 3) / 4095;
    (dens + feel_extra).min(5)
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

/// Extra microtiming ticks at high groove (beyond pattern DNA).
fn groove_timing_boost(feel: u16, groove_max_pct: i32, step: u32) -> i32 {
    let g = groove_feel(feel, groove_max_pct);
    let t = i32::from(feel_curve(g));
    // Odd 16ths push later; some even 16ths pull early — classic pocket.
    let signed = if step % 2 == 1 { 3 } else { -1 };
    (signed * t) / 4095
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

/// Deterministic 0..99 hash from step + voice (no RNG needed).
fn step_chance(step: u32, voice: usize, salt: u32) -> u8 {
    let x = step
        .wrapping_mul(37)
        .wrapping_add(voice as u32)
        .wrapping_mul(17)
        .wrapping_add(salt);
    (x % 100) as u8
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
) -> Option<ResolvedHit> {
    let phrase_bar = bar % PHRASE_BARS;
    let rot = phrase_rot(phrase_bar);
    let hits = rot16(pat.hits, rot);
    let hits_fill = rot16(pat.hits_fill, rot);
    let accent_mask = rot16(pat.accent_mask, rot);
    let si = phrase_degree_si(step, phrase_bar);
    let si_step = step % STEPS_PER_BAR;

    let core = bit_set(hits, si_step);
    let ghost = fill_reveal(hits_fill, density, si_step);
    if !core && ghost.is_none() {
        return None;
    }

    // Cadence breath on bar 8 only — keep the repeating A bars full.
    if phrase_bar == 7
        && core
        && !bit_set(accent_mask, si_step)
        && step_chance(step + phrase_bar * 16, voice_idx, 13) < 65
    {
        return None;
    }
    if phrase_bar == 7 && !core {
        return None;
    }

    let gfeel = groove_feel(feel, groove_max_pct);
    let groove_t = feel_curve(gfeel); // 0..=4095 curved
    // High groove opens Voice ghost/approach character.
    let ghost_pct = voice.ghost_pct as u16
        + ((100u16.saturating_sub(voice.ghost_pct as u16)) * groove_t / 4095) / 2;
    let approach_pct = voice.approach_pct as u16
        + ((100u16.saturating_sub(voice.approach_pct as u16)) * groove_t / 4095) / 3;

    let is_ghost = !core && ghost.is_some();
    if is_ghost {
        let frac = ghost.unwrap_or(0);
        if step_chance(step, voice_idx, 3) >= ghost_pct.min(100) as u8 && frac < 200 && frac < 128
        {
            return None;
        }
    }

    let mut degree = lerp_i32(
        i32::from(pat_lo.degree[si]),
        i32::from(pat_hi.degree[si]),
        g_frac,
    ) as i8;
    if voice.pocket {
        degree = fold_pocket(degree);
    }
    // 8-bar harmony: transpose the line by the phrase chord root.
    let chord = i32::from(PHRASE_PROG[genre.min(NUM_GENRES - 1)][phrase_bar as usize]);
    degree = (i32::from(degree) + chord).rem_euclid(7) as i8;

    // Higher density → allow more adventurous degrees (less folding toward root).
    if density < 1365
        && !voice.pocket
        && degree != 0
        && degree != 4
        && step_chance(step, voice_idx, 9) > 40
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
        && bit_set(accent_mask, si_step)
        && step_chance(step, voice_idx, 5) < 55
    {
        oct = if step_chance(step, voice_idx, 6) < 50 {
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

    // Chromatic approach a semitone below (Voice × Groove).
    if step_chance(step, voice_idx, 11) < approach_pct.min(100) as u8
        && (core || density > 2048 || groove_t > 2048)
    {
        note = note.saturating_sub(1);
    }

    let base = lerp_u8(pat_lo.base_vel, pat_hi.base_vel, g_frac);
    let accent = lerp_u8(pat_lo.accent_vel, pat_hi.accent_vel, g_frac)
        .saturating_add(voice.accent_boost)
        .min(127);
    // High groove widens quiet↔loud spread.
    let quiet_flat = feel_lerp_u16(FLAT_VEL, 45, gfeel);
    let character = if bit_set(accent_mask, si_step) {
        u16::from(accent)
    } else {
        u16::from(base)
    };
    let vel_pct = if is_ghost {
        let g = ghost_vel_pct(ghost.unwrap_or(255), 12, 45);
        feel_lerp_u16(quiet_flat, g, gfeel)
    } else {
        feel_lerp_u16(quiet_flat, character, gfeel)
    };

    let gate_w = lerp_u8(pat_lo.gate_w[si], pat_hi.gate_w[si], g_frac);
    // Low groove → even gates; high groove → Voice staccato character.
    let stacc = feel_lerp_u16(100, u16::from(voice.staccato), gfeel) as u8;
    let gate_w = ((u16::from(gate_w) * u16::from(stacc)) / 100).min(100) as u8;

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
            midi_out: MidiOut::default(),
            groove_max_pct: 80,
            gatel: 100,
            cv_jack: CV_JACK_OUT,
            range: Range::_Neg5_5V,
            vpo: VoltPerOct::Standard,
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
        )
    });

    let mut clock = app.use_clock();
    let ticks = clock.get_ticker();
    let faders = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
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

    let (feel, density, reversed, muted, stored_voice) =
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
    let glob_reversed = app.make_global(reversed);
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
    let glob_reverse_fade = app.make_global(0u16);
    let glob_reverse_fade_up = app.make_global(false);
    let glob_voice_flash = app.make_global(0u16);
    let glob_button_duck = app.make_global(0u16);

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

        loop {
            match clock.wait_for_event(ClockDivision::_1).await {
                ClockEvent::Reset | ClockEvent::Stop => {
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
                        let mut density = if cv_jack == CV_JACK_IN && cv_dest == DEST_DENSITY {
                            mod_u16(glob_density.get(), glob_cv_val.get())
                        } else {
                            glob_density.get()
                        };
                        density = density.saturating_add(voice.syncop_bias).min(4095);

                        let hit = resolve_hit(
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
                        );

                        let core = bit_set(pat.hits, step);
                        let any_ghost = fill_reveal(pat.hits_fill, density, step).is_some();
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

                                // Sustain across empty 16ths until the next hit —
                                // was clamped to one 16th, which made every line staccato.
                                let rot = phrase_rot(bar % PHRASE_BARS);
                                let sust = sustain_sixteenths(
                                    rot16(pat.hits, rot),
                                    rot16(pat.hits_fill, rot),
                                    step,
                                    density,
                                );
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
                _ => {}
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
                    midi.send_note_off(MidiNote::from(n)).await;
                }
                continue;
            }

            if pending_note_off.get() {
                pending_note_off.set(false);
                if let Some(n) = sounding.take() {
                    midi.send_note_off(MidiNote::from(n)).await;
                }
            }

            if pending_note_on.get() {
                pending_note_on.set(false);
                if !glob_muted.get() {
                    let n = pending_note.get();
                    if let Some(prev) = sounding {
                        midi.send_note_off(MidiNote::from(prev)).await;
                    }
                    midi.send_note_on(MidiNote::from(n), pending_vel.get())
                        .await;
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
                buttons.wait_for_up(0).await;
                glob_shift_chord.set(false);
                if !long_press_fired.get() {
                    // Shift + short: reverse swing
                    let reversed = glob_reversed.toggle();
                    storage.modify_and_save(|s| s.reversed = reversed);
                    glob_reverse_fade_up.set(!reversed);
                    glob_reverse_fade.set(REVERSE_FADE_MS);
                }
            } else {
                glob_fader_moved.set(false);
                glob_fader_at_down.set(faders.get_value());
                buttons.wait_for_up(0).await;
                glob_shift_chord.set(false);
                if !long_press_fired.get() {
                    if !glob_fader_moved.get() {
                        glob_reset.set(true);
                    }
                } else if !glob_fader_moved.get() {
                    let muted = glob_muted.toggle();
                    storage.modify_and_save(|s| s.muted = muted);
                    if muted {
                        leds.unset(0, Led::Button);
                        if let Some(ref jack) = out_jack {
                            jack.set_value(0);
                        }
                        pending_note_on.set(false);
                        pending_note_off.set(false);
                        pending_silence.set(true);
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
                let next = (glob_voice.get() + 1) % NUM_VOICES;
                glob_voice.set(next);
                storage.modify_and_save(|s| s.voice = next as u8);
                glob_voice_dirty.set(true);
                glob_voice_flash.set(VOICE_FLASH_MS);
                if !glob_muted.get() {
                    leds.set(
                        0,
                        Led::Button,
                        VOICE_FLASH_COLOR[next],
                        Brightness::High,
                    );
                }
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
                    let (feel, density, reversed, muted, voice) =
                        storage.query(|s| (s.feel, s.density, s.reversed, s.muted, s.voice));
                    glob_feel.set(feel);
                    glob_density.set(density);
                    glob_reversed.set(reversed);
                    glob_muted.set(muted);
                    glob_voice.set((voice as usize).min(NUM_VOICES - 1));
                    let g = params.query(|p| p.genre.min(NUM_GENRES - 1));
                    glob_genre.set(g);
                    glob_genre_fader.set(genre_fader_center(g, NUM_GENRES));
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
                    if glob_reverse_fade.get() == 0 && glob_voice_flash.get() == 0 {
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
            }

            let flash_left = glob_voice_flash.get();
            if flash_left > 0 {
                let left = flash_left.saturating_sub(1);
                glob_voice_flash.set(left);
                if left == 0 && glob_reverse_fade.get() == 0 {
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
                } else if left > 0 && glob_reverse_fade.get() == 0 {
                    // Hold the Voice color for the flash window.
                    let v = glob_voice.get().min(NUM_VOICES - 1);
                    leds.set(0, Led::Button, VOICE_FLASH_COLOR[v], Brightness::High);
                }
            }

            let duck = glob_button_duck.get();
            if duck > 0 {
                glob_button_duck.set(duck.saturating_sub(1));
            }
            if fade_left == 0
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
        }
    };

    join(
        join5(fut_clock, fut_voice, fut_buttons, fut_faders, scene_handler),
        join3(shift, long_press, genre_persist),
    )
    .await;
}
