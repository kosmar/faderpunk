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
    utils::{attenuate_bipolar, split_unsigned_value, value_to_index},
    AppIcon, Brightness, Color, Config, MidiChannel, MidiNote, MidiOut, Param,
    Range, Value, APP_MAX_PARAMS,
};

use crate::app::{
    App, AppParams, AppStorage, Die, Led, ManagedStorage, ParamStore, SceneEvent,
};
use crate::apps::genre_palette::{genre_fader_center, GENRE_NAMES, NUM_GENRES};
use crate::apps::groove::{
    feel_curve, feel_lerp_i32, feel_lerp_u16, swing_bias, swing_delay_ticks, FLAT_VEL, SIXTEENTH,
};
use crate::apps::led_fx::{genre_nearest, genre_pair, lerp_i32, lerp_u8, spectrum_color};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 16;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
/// Mid→Low button duck on each hit — same length as Heat Pump metronome duck.
const BUTTON_DUCK_MS: u16 = 25;
/// Ignore tiny ADC noise when deciding "button+fader scrub" vs long-press mute.
const FADER_MOVE_THRESH: u16 = 64;
/// No clock tick advance for this long → idle LEDs (genre color on Top).
const CLOCK_STALL_MS: u16 = 100;

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

/// Morph axis — indices match Shift+Fader buckets and Enum param.
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
    // 2 House — classic 4-on-floor, clap 2&4
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
    // 3 Techno — straight kick; Feel via hat dynamics
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
    // 4 Trip-Hop — laid-back, late snare pocket
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
    // 5 Hip-Hop — boom-bap; strong 1+3 kick accents
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
    // 6 Jungle — amen-ish breakbeat; busy kick, snare 2&4, rapid hats
    Pattern {
        kick: 0b0100_1001_0010_0101,
        snare: 0b0001_0000_0001_0000,
        hats: 0b1110_1010_1110_1010,
        kick_fill: 0b0010_0000_0100_1000,
        snare_fill: 0b0100_0010_0100_0010,
        hats_fill: 0b1111_1111_1111_1111,
        kick_base: 55,
        kick_accent: 100,
        kick_acc_mask: 0b0000_0001_0000_0001,
        snare_base: 50,
        snare_accent: 100,
        snare_acc_mask: 0b0001_0000_0001_0000,
        hats_base: 40,
        hats_accent: 88,
        hats_acc_mask: 0b1010_0000_1010_0000,
        timing: [0, 3, -1, 2, 1, 3, 0, 4, 0, 3, -1, 2, 1, 3, 0, 4],
    },
    // 7 UK Garage — skippy kick/hats
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
    // 8 Dubstep — half-time: kick 1, snare 3
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
})
.add_param(Param::Enum {
    name: "Jack Mode",
    variants: &["Any", "Stacked"],
})
.add_param(Param::Enum {
    name: "Swing Dir",
    variants: &["Normal", "Reverse"],
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
    midi_out: MidiOut,
    cv_jack: usize,
    range: Range,
    cv_dest: usize,
    cv_att: i32,
    /// CV Out activity: Any (OR) vs Stacked (level by voice count).
    jack_mode: usize,
    /// Swing direction: Normal (offbeats late) vs Reverse (offbeats early).
    swing_dir: usize,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        // Layout without Color (current): [0..=8 core] [9 midi_out] [10..=13 cv…] [14 jack_mode]
        // Legacy with Color at [9]:       [0..=8 core] [9 color] [10 midi_out] [11..=14 cv…] [15 jack_mode]
        // Pre-jack-mode lists omit the final enum and default to Any.
        // Length 15 is ambiguous (old color layout vs new + jack_mode) — discriminate on values[9].
        let legacy_color =
            values.len() == 11 || matches!(values.get(9), Some(Value::Color(_)));
        let midi_i = if legacy_color { 10 } else { 9 };
        let cv_i = midi_i + 1;
        if values.len() < midi_i + 1 {
            return None;
        }
        let (cv_jack, range, cv_dest, cv_att) = if values.len() >= cv_i + 4 {
            (
                usize::from_value(values[cv_i]).min(1),
                Range::from_value(values[cv_i + 1]),
                usize::from_value(values[cv_i + 2]).min(DEST_COUNT - 1),
                i32::from_value(values[cv_i + 3]).clamp(0, 100),
            )
        } else {
            (CV_JACK_OUT, Range::_0_10V, DEST_DENSITY, 100)
        };
        let jack_mode_i = cv_i + 4;
        let jack_mode = if values.len() > jack_mode_i {
            usize::from_value(values[jack_mode_i]).min(1)
        } else {
            JACK_ANY as usize
        };
        let swing_dir_i = jack_mode_i + 1;
        let swing_dir = if values.len() > swing_dir_i {
            usize::from_value(values[swing_dir_i]).min(1)
        } else {
            0
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
            midi_out: MidiOut::from_value(values[midi_i]),
            cv_jack,
            range,
            cv_dest,
            cv_att,
            jack_mode,
            swing_dir,
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
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.cv_jack.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.cv_dest.into()).unwrap();
        vec.push(self.cv_att.into()).unwrap();
        vec.push(self.jack_mode.into()).unwrap();
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
    /// Groove feel attenuator (0 = flat/grid, 4095 = full genre character).
    /// Same FRAM slot as the former `swing` field.
    feel: u16,
    /// Groove density: progressively reveals extra kick/snare/hat hits
    /// across the whole pattern (not just hats) as this rises.
    density: u16,
    /// Legacy scene fields — Jack Mode and swing direction now live in Params;
    /// kept for FRAM shape.
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

/// A full-bar fill / break figure. Bit N = 16th step N, same layout as
/// [`Pattern`]. Held gestures play whatever slice of the bar they cover, so the
/// figures are written to build toward the downbeat.
struct Fill {
    kick: u16,
    snare: u16,
    hats: u16,
}

const FILL_VARIANTS: usize = 3;

/// Dedicated fill bars per genre. These are deliberately *not* the density
/// `*_fill` masks: those only add whatever the reveal has not consumed yet, so a
/// fill could land silent with the Density fader parked at either end.
const FILLS: [[Fill; FILL_VARIANTS]; NUM_GENRES] = [
    // 0 Dub — dubby space, snare answers late
    [
        Fill { kick: 0b0000_0100_0000_0001, snare: 0b1001_0000_0001_0000, hats: 0b0100_0000_0100_0100 },
        Fill { kick: 0b1000_0001_0000_0001, snare: 0b0011_0000_0001_0000, hats: 0b0100_0000_0100_0000 },
        Fill { kick: 0b0001_0000_0000_0001, snare: 0b1100_0000_0001_0000, hats: 0b1010_0100_0100_0100 },
    ],
    // 1 Disco — snare roll over a surviving four-on-floor
    [
        Fill { kick: 0b0001_0001_0001_0001, snare: 0b1111_0000_0001_0000, hats: 0b1111_0101_0101_0101 },
        Fill { kick: 0b0101_0001_0001_0001, snare: 0b1011_0000_0001_0000, hats: 0b1111_0100_0100_0100 },
        Fill { kick: 0b0001_0001_0001_0001, snare: 0b1111_0000_0001_0000, hats: 0b1000_0000_0000_0000 },
    ],
    // 2 House — clap/snare interplay, driving kick
    [
        Fill { kick: 0b0101_0001_0001_0001, snare: 0b1011_0000_0001_0000, hats: 0b1111_0101_0101_0101 },
        Fill { kick: 0b0001_0001_0001_0001, snare: 0b1111_0000_0001_0000, hats: 0b1111_0100_0100_0100 },
        Fill { kick: 0b1001_0001_0001_0001, snare: 0b0110_0000_0001_0000, hats: 0b1010_0100_0100_0100 },
    ],
    // 3 Techno — machine-gun, straight
    [
        Fill { kick: 0b1111_0001_0001_0001, snare: 0b0000_0000_0000_0000, hats: 0b1111_1111_1111_1111 },
        Fill { kick: 0b0101_0001_0001_0001, snare: 0b1010_0000_0000_0000, hats: 0b1111_0101_0101_0101 },
        Fill { kick: 0b0001_0001_0001_0001, snare: 0b1110_0000_0000_0000, hats: 0b1111_1111_1111_1111 },
    ],
    // 4 Trip-Hop — dragging, half-speed figures
    [
        Fill { kick: 0b0000_0100_0000_0001, snare: 0b1011_0000_0001_0000, hats: 0b0100_0000_0100_0000 },
        Fill { kick: 0b0000_0001_0000_0001, snare: 0b1101_0000_0001_0000, hats: 0b1010_0100_0100_0100 },
        Fill { kick: 0b0101_0100_0000_0001, snare: 0b1000_0000_0001_0000, hats: 0b0010_0000_0100_0000 },
    ],
    // 5 Hip-Hop — boom-bap snare roll
    [
        Fill { kick: 0b0001_0001_0000_1001, snare: 0b1111_0000_0001_0000, hats: 0b1111_0101_0101_0101 },
        Fill { kick: 0b0100_0100_0000_1001, snare: 0b1011_0000_0001_0000, hats: 0b1010_0100_0100_0100 },
        Fill { kick: 0b0000_0001_0000_0001, snare: 0b1111_0000_0001_0000, hats: 0b0101_0001_0001_0001 },
    ],
    // 6 Jungle — amen chop, busy interleave
    [
        Fill { kick: 0b0010_1001_0000_1001, snare: 0b1101_0100_0001_0000, hats: 0b1111_1111_1111_1111 },
        Fill { kick: 0b0100_0100_0100_0001, snare: 0b1011_0001_0001_0000, hats: 0b1111_1111_1111_1111 },
        Fill { kick: 0b0001_0000_0000_0001, snare: 0b1111_0100_0001_0000, hats: 0b1010_0100_0100_0100 },
    ],
    // 7 UK Garage — skippy, syncopated
    [
        Fill { kick: 0b0101_0100_0100_0001, snare: 0b1010_0000_0001_0000, hats: 0b1111_1111_1111_1111 },
        Fill { kick: 0b0011_0000_0100_0001, snare: 0b1100_0000_0001_0000, hats: 0b1100_1101_0100_1101 },
        Fill { kick: 0b0110_0000_0100_0001, snare: 0b1001_0000_0001_0000, hats: 0b1111_1111_1111_1111 },
    ],
    // 8 Dubstep — half-time, few but heavy
    [
        Fill { kick: 0b0010_0000_0000_0001, snare: 0b1000_0001_0000_0000, hats: 0b0101_0000_0100_0100 },
        Fill { kick: 0b0001_0000_0000_0001, snare: 0b1100_0001_0000_0000, hats: 0b1010_0000_0100_0000 },
        Fill { kick: 0b0100_0000_0000_0001, snare: 0b1010_0001_0000_0000, hats: 0b0001_0000_0000_0000 },
    ],
];

/// One bar of a held solo. Bit N = 16th step N (same layout as [`Pattern`]); the
/// `*_acc` masks mark where the weight lands. Density alone reads as a machine
/// gun, so every figure keeps rests, trades kick against snare instead of
/// stacking them, and carries its own accent architecture.
struct Solo {
    kick: u16,
    snare: u16,
    hats: u16,
    kick_acc: u16,
    snare_acc: u16,
    hats_acc: u16,
}

/// Intensity tiers a held gesture walks through: open build → busier → peak.
const SOLO_TIERS: usize = 3;

/// Bars per solo phrase: the three tiers, then a turnaround bar that falls back
/// to the end-weighted [`FILLS`] figure so the phrase breathes and resolves onto
/// the one instead of sitting at peak intensity forever.
const SOLO_PHRASE_BARS: u32 = 4;

const SOLOS: [[Solo; SOLO_TIERS]; NUM_GENRES] = [
    // 0 Dub — echo answers around a wide-open bar
    [
        Solo {
            kick: 0b0001_0001_1000_0001,
            snare: 0b0101_0000_0001_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0000_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0001_0001_1000_1001,
            snare: 0b0101_0100_0001_0000,
            hats: 0b0101_0100_0101_0100,
            kick_acc: 0b0000_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0010_1001_0100_1001,
            snare: 0b1001_0100_0001_0000,
            hats: 0b0101_0101_0101_0100,
            kick_acc: 0b0000_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
    // 1 Disco — snare bursts over a surviving four-on-floor
    [
        Solo {
            kick: 0b0001_0001_0001_0001,
            snare: 0b1001_0000_1001_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0001_0001_0001_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0101_0001_0001_0001,
            snare: 0b1011_0000_1001_0000,
            hats: 0b0101_0101_0101_0100,
            kick_acc: 0b0001_0001_0001_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0101_0001_0001_0001,
            snare: 0b1001_1000_1001_1000,
            hats: 0b0011_0101_0011_0101,
            kick_acc: 0b0001_0001_0001_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0001_0000_0001_0000,
        },
    ],
    // 2 House — clap answers, kick never leaves the floor
    [
        Solo {
            kick: 0b0001_0001_0001_0001,
            snare: 0b0001_1000_0001_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0001_0001_0001_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0001_0001_1001_0001,
            snare: 0b1001_0100_0001_0000,
            hats: 0b0101_0101_0101_0100,
            kick_acc: 0b0001_0001_0001_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b1001_0001_1001_0001,
            snare: 0b0101_0100_0001_1000,
            hats: 0b1101_0101_1101_0101,
            kick_acc: 0b0001_0001_0001_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
    // 3 Techno — 16th kick bursts trading with clap stabs
    [
        Solo {
            kick: 0b0001_0001_0001_0001,
            snare: 0b0001_0000_0000_0000,
            hats: 0b0101_0101_0101_0101,
            kick_acc: 0b0001_0001_0001_0001,
            snare_acc: 0b0001_0000_0000_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0001_0001_0000_1111,
            snare: 0b0001_0000_0001_0000,
            hats: 0b1111_0101_0101_0101,
            kick_acc: 0b0001_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0000_1111_0000_1111,
            snare: 0b0111_0000_0001_0000,
            hats: 0b0101_0101_0101_0101,
            kick_acc: 0b0000_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
    // 4 Trip-Hop — half-speed drag, ghosts behind the beat
    [
        Solo {
            kick: 0b0000_0100_0000_0001,
            snare: 0b0001_0000_0001_0000,
            hats: 0b0100_0000_0100_0000,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0000_0100_0000_1001,
            snare: 0b0101_0000_0001_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0010_0100_0000_1001,
            snare: 0b0101_0000_1001_0000,
            hats: 0b0100_0101_0100_0100,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
    // 5 Hip-Hop — boom-bap, snare rolls out of the gap
    [
        Solo {
            kick: 0b0000_0001_0000_1001,
            snare: 0b0001_0000_0001_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0000_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0000_1001_0000_1001,
            snare: 0b1001_0000_1001_0000,
            hats: 0b0101_0101_0101_0101,
            kick_acc: 0b0000_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0100_1001_0000_1001,
            snare: 0b1011_0000_0011_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0000_0001_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
    // 6 Jungle — amen chop: kick on 1 and 3e, snare answers on the e
    [
        Solo {
            kick: 0b0000_0100_0000_0001,
            snare: 0b1001_0000_1001_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0000_0100_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0000_0100_0000_1001,
            snare: 0b1001_1000_1001_0000,
            hats: 0b0101_0100_0101_0100,
            kick_acc: 0b0000_0100_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0000_0101_0000_1001,
            snare: 0b1101_1000_1011_0000,
            hats: 0b0101_0100_0101_0101,
            kick_acc: 0b0000_0100_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
    // 7 UK Garage — 2-step skip, snare shuffles around the backbeat
    [
        Solo {
            kick: 0b0000_0100_0100_0001,
            snare: 0b0001_0000_0001_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0100_0100_0100_0001,
            snare: 0b0001_1000_0001_1000,
            hats: 0b0101_0101_0101_0100,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0100_0101_0100_0001,
            snare: 0b1001_1000_1001_1000,
            hats: 0b0101_0101_0101_0101,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0001_0000_0001_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
    // 8 Dubstep — half-time anchor, syncopation around the weight
    [
        Solo {
            kick: 0b0000_0000_0100_0001,
            snare: 0b0000_0001_0000_0000,
            hats: 0b0100_0100_0100_0100,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0000_0001_0000_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0000_0100_0100_0001,
            snare: 0b1000_0001_0000_0000,
            hats: 0b0101_0100_0100_0100,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0000_0001_0000_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
        Solo {
            kick: 0b0010_0100_0100_0001,
            snare: 0b1001_0001_0000_0000,
            hats: 0b0101_0100_0101_0100,
            kick_acc: 0b0000_0000_0000_0001,
            snare_acc: 0b0000_0001_0000_0000,
            hats_acc: 0b0100_0000_0100_0000,
        },
    ],
];

/// Break bars: the same gesture read subtractively. Chosen once the groove is
/// already dense, where piling on more hits reads as mush — pulling the floor
/// away and resolving on the downbeat is the louder move.
const BREAKS: [[Fill; FILL_VARIANTS]; NUM_GENRES] = [
    // 0 Dub — near-total silence
    [
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1100_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0001_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
    ],
    // 1 Disco — drop to the kick, then answer
    [
        Fill { kick: 0b0001_0001_0001_0001, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0100_0100_0100_0100 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1100_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
    ],
    // 2 House — filter-break hats
    [
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0100_0100_0100_0100 },
        Fill { kick: 0b0001_0001_0001_0001, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
    ],
    // 3 Techno — kick-only drop
    [
        Fill { kick: 0b0001_0001_0001_0001, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0001_0000_0001, snare: 0b0000_0000_0000_0000, hats: 0b1100_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0101_0101_0101_0101 },
    ],
    // 4 Trip-Hop — hang back, late answer
    [
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1100_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0001_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
    ],
    // 5 Hip-Hop — snare pickup out of the gap
    [
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1110_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0001_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1100_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
    ],
    // 6 Jungle — chopped silence
    [
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1100_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0001_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1001_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
    ],
    // 7 UK Garage — skippy stub
    [
        Fill { kick: 0b0000_0000_0100_0001, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1100_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0100_0100_0000_0000 },
    ],
    // 8 Dubstep — maximum air
    [
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0000, snare: 0b1100_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
        Fill { kick: 0b0000_0000_0000_0001, snare: 0b1000_0000_0000_0000, hats: 0b0000_0000_0000_0000 },
    ],
];

/// Density at which the gesture flips from additive fill to subtractive break.
const BREAK_DENSITY: u16 = 2400;

/// Deterministic 0..99 hash so low Feel keeps one signature figure per genre.
fn fill_hash(genre: usize) -> u8 {
    let x = (genre as u32)
        .wrapping_mul(2_654_435_761)
        .wrapping_add(0x9E37_79B9);
    ((x >> 9) % 100) as u8
}

/// Feel-weighted variant roll: low Feel stays hash-locked, high Feel leans on the
/// live Die — same groove-weighted mix Bassment uses for its ghosts.
fn fill_variant(die: &Die, genre: usize, feel: u16) -> usize {
    let hashed = u32::from(fill_hash(genre));
    let live = u32::from(die.roll() % 100);
    // Cap the live mix at ~80% so the genre's own figure still peeks through.
    let w = (u32::from(feel_curve(feel)) * 4) / 5;
    let roll = (hashed * (4095 - w) + live * w) / 4095;
    ((roll as usize * FILL_VARIANTS) / 100).min(FILL_VARIANTS - 1)
}

/// Fill velocity: crescendo from `base` toward `accent` across the bar so a
/// figure drives into the downbeat instead of ghosting under the groove.
fn fill_vel_pct(base: u8, accent: u8, step: u32) -> u16 {
    let b = u16::from(base);
    let a = u16::from(accent).max(b);
    let idx = (step % STEPS_PER_BAR) as u16;
    b + ((a - b) * idx) / (STEPS_PER_BAR as u16 - 1)
}

/// Solo velocity: the figure's own accent mask decides the weight — that accent
/// architecture *is* the groove, so Feel only adds human jitter around it rather
/// than randomising the dynamics. Ghost fill-ins sit well under the written hits.
fn solo_vel_pct(base: u8, accent: u8, acc: bool, ghost: bool, die: &Die, feel: u16) -> u16 {
    let b = u16::from(base);
    let a = u16::from(accent).max(b);
    let weight = if acc { a } else { b };
    let target = if ghost { (weight * 2) / 5 } else { weight };
    let jitter = (u32::from(feel_curve(feel)) * 12) / 4095;
    let off = u32::from(die.roll()) % (2 * jitter + 1);
    (u32::from(target) + off)
        .saturating_sub(jitter)
        .clamp(1, 100) as u16
}

/// Ghost fill-ins while soloing. Only fires on the step right after that voice
/// already hit, so it reads as a drag/flam off the written figure instead of a
/// random extra note. Salt keeps the three voices from ghosting in lockstep.
fn solo_ghost(die: &Die, feel: u16, prev_hit: bool, salt: u8) -> bool {
    if !prev_hit {
        return false;
    }
    let open = u32::from(feel_curve(feel));
    // ~4% at Feel=0 … ~20% at Feel=max.
    let chance = 4 + (open * 16) / 4095;
    u32::from((die.roll() ^ u16::from(salt)) % 100) < chance
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
            genre: 2, // House
            swing_max_pct: 50,
            // Fraction of a 16th (same GATE % convention as Euclid/Turing).
            gatel: 100,
            midi_out: MidiOut([true, false, false]), // USB only — all-ports floods cable
            cv_jack: CV_JACK_OUT,
            range: Range::_0_10V,
            cv_dest: DEST_DENSITY,
            cv_att: 100,
            jack_mode: JACK_ANY as usize,
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
        note_kick,
        note_snare,
        note_hats,
        midi_channel_kick,
        midi_channel_snare,
        midi_channel_hats,
        genre,
        swing_max_pct,
        gatel,
        cv_jack,
        range,
        cv_dest,
        cv_att,
        jack_mode,
        swing_dir,
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
            p.cv_jack.min(1),
            p.range,
            p.cv_dest.min(DEST_COUNT - 1),
            att_from_pct(p.cv_att),
            (p.jack_mode.min(1) as u8),
            p.swing_dir == 1,
        )
    });

    // Ticker only — never CLOCK_PUBSUB (Grooves+Vamp+Bassment+Contura combo).
    let ticks = app.clock_ticker();
    let faders = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
    let die = app.use_die();
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

    let (feel, density, muted) = storage.query(|s| (s.feel, s.density, s.muted));

    let glob_feel = app.make_global(feel);
    let glob_swing_max = app.make_global(swing_max_pct);
    let glob_density = app.make_global(density);
    // Jack Mode and swing direction are Config params — scenes no longer own them.
    let glob_jack_mode = app.make_global(jack_mode);
    let glob_reversed = app.make_global(swing_dir);
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
    // Scene storage: globals only on fader/button path — never storage.modify
    // while saver_task may borrow_mut (RefCell panic under ADC noise / mute).
    let glob_storage_dirty = app.make_global(false);
    let glob_latch_layer = app.make_global(LatchLayer::Main);
    // Fill/break/solo gesture: Shift tap = genre fill (or break at high density)
    // until the next downbeat. Shift hold escalates to a denser solo thunderstorm
    // that re-rolls each bar; release keeps it open until the one so it resolves.
    let glob_fill_armed = app.make_global(false);
    let glob_fill_held = app.make_global(false);
    let glob_fill_start = app.make_global(false);
    let glob_fill_variant = app.make_global(0usize);
    let glob_fill_break = app.make_global(false);
    let glob_fill_solo = app.make_global(false);
    let glob_button_duck = app.make_global(0u16);
    // Clock watch → voice engine (never await MIDI inside the clock subscriber).
    // Same isolation as Chord Vamp / Arp — keeps Harmonica / Note Fader MIDI
    // storms from stalling CLOCK_PUBSUB and dropping 16ths.
    let pending_silence = app.make_global(false);
    let pending_note_off = app.make_global(false);
    let pending_kick = app.make_global(false);
    let pending_snare = app.make_global(false);
    let pending_hats = app.make_global(false);
    let pending_kick_vel = app.make_global(0u16);
    let pending_snare_vel = app.make_global(0u16);
    let pending_hats_vel = app.make_global(0u16);

    // Clear any hanging notes from a prior respawn.
    midi_kick.send_note_off(note_kick).await;
    midi_snare.send_note_off(note_snare).await;
    midi_hats.send_note_off(note_hats).await;

    if muted {
        leds.unset(0, Led::Button);
        leds.unset(0, Led::Top);
        leds.unset(0, Led::Bottom);
    } else {
        let color = spectrum_color(glob_genre_fader.get());
        leds.set(0, Led::Button, color, LED_BRIGHTNESS);
        // Idle presence until the first clock tick (Top was otherwise empty).
        leds.set(0, Led::Top, color, LED_BRIGHTNESS);
        leds.unset(0, Led::Bottom);
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
        // GATE % = fraction of a 16th (Euclid/Turing convention). Feel/swing only
        // move the attack; `room` below still caps so late hits never smear into
        // the next 16th (that looked like Chord Vamp holds).
        let gate_len = ((SIXTEENTH as i32 * gatel) / 100)
            .clamp(1, (SIXTEENTH as i32) - 1) as u32;

        let mut last_tick = ticks();
        let mut stall_ms = 0u16;
        // Slot the current fill/break gesture started on, so the release resolves
        // on the *next* downbeat rather than the one it may have started on.
        let mut fill_start_slot = 0u32;
        // Bars elapsed in the current solo, walking SOLO_PHRASE_BARS.
        let mut solo_bar = 0u32;
        let mut was_solo = false;

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

                    // Flag only — voice owns MIDI so we keep draining clock ticks.
                    pending_kick.set(false);
                    pending_snare.set(false);
                    pending_hats.set(false);
                    pending_note_off.set(false);
                    pending_silence.set(true);
                    kick_on = false;
                    snare_on = false;
                    hats_on = false;
                    if let Some(ref jack) = out_jack {
                        jack.set_value(pulse_idle(range));
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
                        // Keep genre color visible while clock is stopped.
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

                    let density = if cv_jack == CV_JACK_IN && cv_dest == DEST_DENSITY {
                        mod_u16(glob_density.get(), glob_cv_val.get())
                    } else {
                        glob_density.get()
                    };

                    // Fill/break/solo gesture. Figure and additive-vs-subtractive
                    // reading lock in at press so a tap stays one phrase. Holding
                    // across a bar line escalates into the solo, which then walks
                    // its phrase instead of looping one end-of-bar fill. Arm
                    // outlives the release until the bar wraps so it resolves.
                    if glob_fill_start.get() {
                        glob_fill_start.set(false);
                        glob_fill_variant.set(fill_variant(&die, near, feel_val));
                        glob_fill_break.set(density >= BREAK_DENSITY);
                        glob_fill_solo.set(false);
                        glob_fill_armed.set(true);
                        fill_start_slot = slot;
                    } else if glob_fill_armed.get() && step == 0 && slot > fill_start_slot {
                        if glob_fill_held.get() {
                            // Still holding past a bar line → solo, one bar
                            // further into the phrase each time round.
                            if glob_fill_solo.get() {
                                solo_bar = solo_bar.wrapping_add(1);
                            } else {
                                glob_fill_solo.set(true);
                                solo_bar = 0;
                            }
                            glob_fill_variant.set(fill_variant(&die, near, feel_val));
                            fill_start_slot = slot;
                        } else {
                            glob_fill_armed.set(false);
                            glob_fill_solo.set(false);
                        }
                    }
                    // A long press escalates mid-bar; start its phrase there.
                    let solo_now = glob_fill_armed.get() && glob_fill_solo.get();
                    if solo_now && !was_solo {
                        solo_bar = 0;
                    }
                    was_solo = solo_now;

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
                            if kick_on || snare_on || hats_on {
                                // Cancel any unsent note-ons so a stalled voice
                                // does not fire a hit after its gate expired.
                                pending_kick.set(false);
                                pending_snare.set(false);
                                pending_hats.set(false);
                                pending_note_off.set(true);
                                kick_on = false;
                                snare_on = false;
                                hats_on = false;
                            }
                            if let Some(ref jack) = out_jack {
                                jack.set_value(pulse_idle(range));
                            }
                            gate_off_at = None;
                            leds.set(0, Led::Bottom, spectrum_color(glob_genre_fader.get()), Brightness::Off);
                        }
                    }

                    // Fire-once guard: a feel/density change mid-window
                    // can't skip a step or fire it twice.
                    if slot != last_fired_slot && !glob_muted.get() {
                        // While armed the figure replaces the groove outright —
                        // density reveal plays no part, so the gesture reads the
                        // same at any fader position. A held gesture walks the
                        // solo tiers; the phrase's turnaround bar and the release
                        // both fall back to the end-weighted FILLS so the solo
                        // breathes and lands on the one.
                        let tier = (solo_bar % SOLO_PHRASE_BARS) as usize;
                        let solo_fig = if glob_fill_armed.get()
                            && glob_fill_solo.get()
                            && glob_fill_held.get()
                            && tier < SOLO_TIERS
                        {
                            Some(&SOLOS[near][tier])
                        } else {
                            None
                        };
                        let fill_fig = if glob_fill_armed.get() && solo_fig.is_none() {
                            let v = glob_fill_variant.get().min(FILL_VARIANTS - 1);
                            Some(if glob_fill_break.get() && !glob_fill_solo.get() {
                                &BREAKS[near][v]
                            } else {
                                &FILLS[near][v]
                            })
                        } else {
                            None
                        };
                        let gesture = solo_fig.is_some() || fill_fig.is_some();

                        let mut kick_acc_hit = false;
                        let mut snare_acc_hit = false;
                        let mut hats_acc_hit = false;
                        let mut kick_gh = false;
                        let mut snare_gh = false;
                        let mut hats_gh = false;

                        let (kick_core, snare_core, hats_core) = if let Some(f) = solo_fig {
                            kick_acc_hit = bit_set(f.kick_acc, step);
                            snare_acc_hit = bit_set(f.snare_acc, step);
                            hats_acc_hit = bit_set(f.hats_acc, step);
                            let mut k = bit_set(f.kick, step);
                            let mut s = bit_set(f.snare, step);
                            let mut h = bit_set(f.hats, step);
                            // Drag off the written hits, bar-cyclic so step 0
                            // answers step 15.
                            let prev = (step + STEPS_PER_BAR - 1) % STEPS_PER_BAR;
                            if !k && solo_ghost(&die, feel_val, bit_set(f.kick, prev), 0x11) {
                                k = true;
                                kick_gh = true;
                            }
                            if !s && solo_ghost(&die, feel_val, bit_set(f.snare, prev), 0x22) {
                                s = true;
                                snare_gh = true;
                            }
                            if !h && solo_ghost(&die, feel_val, bit_set(f.hats, prev), 0x33) {
                                h = true;
                                hats_gh = true;
                            }
                            (k, s, h)
                        } else if let Some(f) = fill_fig {
                            (
                                bit_set(f.kick, step),
                                bit_set(f.snare, step),
                                bit_set(f.hats, step),
                            )
                        } else {
                            (
                                bit_set(pat.kick, step),
                                bit_set(pat.snare, step),
                                bit_set(pat.hats, step),
                            )
                        };
                        // `Some(frac)`: this step's fill bit for that voice is
                        // being progressively revealed by the density fader —
                        // frac (0..=255) is how far in it's faded (continuum,
                        // no hard zone jumps).
                        let (kick_ghost, snare_ghost, hats_ghost) = if gesture {
                            (None, None, None)
                        } else {
                            (
                                fill_reveal(pat.kick_fill, density, step),
                                fill_reveal(pat.snare_fill, density, step),
                                fill_reveal(pat.hats_fill, density, step),
                            )
                        };

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
                                pending_kick.set(false);
                                pending_snare.set(false);
                                pending_hats.set(false);
                                pending_note_off.set(true);
                                kick_on = false;
                                snare_on = false;
                                hats_on = false;
                                gate_off_at = None;
                            }

                            if do_kick {
                                let v = if solo_fig.is_some() {
                                    solo_vel_pct(
                                        lerp_u8(pat_lo.kick_base, pat_hi.kick_base, g_frac),
                                        lerp_u8(pat_lo.kick_accent, pat_hi.kick_accent, g_frac),
                                        kick_acc_hit,
                                        kick_gh,
                                        &die,
                                        feel_val,
                                    )
                                } else if fill_fig.is_some() {
                                    fill_vel_pct(
                                        lerp_u8(pat_lo.kick_base, pat_hi.kick_base, g_frac),
                                        lerp_u8(pat_lo.kick_accent, pat_hi.kick_accent, g_frac),
                                        step,
                                    )
                                } else {
                                    match kick_ghost {
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
                                    }
                                };
                                pending_kick_vel.set(midi_vel(v));
                                pending_kick.set(true);
                                kick_on = true;
                            }
                            if do_snare {
                                let v = if solo_fig.is_some() {
                                    solo_vel_pct(
                                        lerp_u8(pat_lo.snare_base, pat_hi.snare_base, g_frac),
                                        lerp_u8(pat_lo.snare_accent, pat_hi.snare_accent, g_frac),
                                        snare_acc_hit,
                                        snare_gh,
                                        &die,
                                        feel_val,
                                    )
                                } else if fill_fig.is_some() {
                                    fill_vel_pct(
                                        lerp_u8(pat_lo.snare_base, pat_hi.snare_base, g_frac),
                                        lerp_u8(pat_lo.snare_accent, pat_hi.snare_accent, g_frac),
                                        step,
                                    )
                                } else {
                                    match snare_ghost {
                                        Some(frac) if !snare_core => {
                                            let g = ghost_vel_pct(frac, 15, 32);
                                            feel_lerp_u16(FLAT_VEL, g, feel_val)
                                        }
                                        _ => core_vel_pct(
                                            lerp_u8(pat_lo.snare_base, pat_hi.snare_base, g_frac),
                                            lerp_u8(
                                                pat_lo.snare_accent,
                                                pat_hi.snare_accent,
                                                g_frac,
                                            ),
                                            pat.snare_acc_mask,
                                            step,
                                            feel_val,
                                        ),
                                    }
                                };
                                pending_snare_vel.set(midi_vel(v));
                                pending_snare.set(true);
                                snare_on = true;
                            }
                            if do_hats {
                                let v = if solo_fig.is_some() {
                                    solo_vel_pct(
                                        lerp_u8(pat_lo.hats_base, pat_hi.hats_base, g_frac),
                                        lerp_u8(pat_lo.hats_accent, pat_hi.hats_accent, g_frac),
                                        hats_acc_hit,
                                        hats_gh,
                                        &die,
                                        feel_val,
                                    )
                                } else if fill_fig.is_some() {
                                    fill_vel_pct(
                                        lerp_u8(pat_lo.hats_base, pat_hi.hats_base, g_frac),
                                        lerp_u8(pat_lo.hats_accent, pat_hi.hats_accent, g_frac),
                                        step,
                                    )
                                } else {
                                    match hats_ghost {
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
                                    }
                                };
                                pending_hats_vel.set(midi_vel(v));
                                pending_hats.set(true);
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
                                // attacks still get a shortened gate, not a smear.
                                let room = SIXTEENTH.saturating_sub(phase).max(1);
                                let pulse = gate_len.min(room);
                                gate_off_at = Some(clkn.wrapping_add(pulse));
                                leds.set(0, Led::Bottom, spectrum_color(glob_genre_fader.get()), Brightness::High);
                                glob_button_duck.set(BUTTON_DUCK_MS);
                            }
                        }
                    }

                    // Top LED: bar progress on Main (needs step). Alt/Third
                    // genre+Feel previews live in the 1 ms shift loop so the
                    // spectrum tracks the fader fluidly (and still works when
                    // the clock is stopped) — same as Chord Vamp.
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

    // MIDI voice engine — isolated from the clock subscriber so APP_MIDI_CHANNEL
    // backpressure (Harmonica chord storms, Note Fader spam) cannot stall ticks.
    let fut_voice = async {
        let mut kick_sounding = false;
        let mut snare_sounding = false;
        let mut hats_sounding = false;
        loop {
            app.delay_millis(1).await;

            if pending_silence.get() {
                pending_silence.set(false);
                pending_note_off.set(false);
                pending_kick.set(false);
                pending_snare.set(false);
                pending_hats.set(false);
                if kick_sounding {
                    midi_kick.try_send_note_off(note_kick);
                    kick_sounding = false;
                }
                if snare_sounding {
                    midi_snare.try_send_note_off(note_snare);
                    snare_sounding = false;
                }
                if hats_sounding {
                    midi_hats.try_send_note_off(note_hats);
                    hats_sounding = false;
                }
                continue;
            }

            // Off before on in the same poll — re-triggers set both flags.
            if pending_note_off.get() {
                pending_note_off.set(false);
                if kick_sounding {
                    midi_kick.try_send_note_off(note_kick);
                    kick_sounding = false;
                }
                if snare_sounding {
                    midi_snare.try_send_note_off(note_snare);
                    snare_sounding = false;
                }
                if hats_sounding {
                    midi_hats.try_send_note_off(note_hats);
                    hats_sounding = false;
                }
            }

            if pending_kick.get() {
                pending_kick.set(false);
                if !glob_muted.get() {
                    if kick_sounding {
                        midi_kick.try_send_note_off(note_kick);
                    }
                    midi_kick.try_send_note_on(note_kick, pending_kick_vel.get());
                    kick_sounding = true;
                }
            }
            if pending_snare.get() {
                pending_snare.set(false);
                if !glob_muted.get() {
                    if snare_sounding {
                        midi_snare.try_send_note_off(note_snare);
                    }
                    midi_snare.try_send_note_on(note_snare, pending_snare_vel.get());
                    snare_sounding = true;
                }
            }
            if pending_hats.get() {
                pending_hats.set(false);
                if !glob_muted.get() {
                    if hats_sounding {
                        midi_hats.try_send_note_off(note_hats);
                    }
                    midi_hats.try_send_note_on(note_hats, pending_hats_vel.get());
                    hats_sounding = true;
                }
            }
        }
    };

    let fut_buttons = async {
        loop {
            buttons.wait_for_any_down().await;
            if buttons.is_shift_pressed() {
                long_press_fired.set(false);
                // Shift+tap: genre fill (or break) until the next downbeat.
                // Shift+hold across a bar: escalates to a solo thunderstorm.
                // Clock keeps either gesture open past release until the one.
                glob_fill_held.set(true);
                glob_fill_start.set(true);
                buttons.wait_for_up(0).await;
                glob_fill_held.set(false);
            } else {
                long_press_fired.set(false);
                glob_fader_moved.set(false);
                glob_fader_at_down.set(faders.get_value());
                buttons.wait_for_up(0).await;
                // Short: mute — same as Contura / Bassment. Reset stays on Long
                // (and on CV Dest: Reset).
                if !long_press_fired.get() && !glob_fader_moved.get() {
                    let muted = glob_muted.toggle();
                    glob_storage_dirty.set(true);
                    if muted {
                        leds.unset(0, Led::Button);
                        if let Some(ref jack) = out_jack {
                            jack.set_value(pulse_idle(range));
                        }
                        pending_kick.set(false);
                        pending_snare.set(false);
                        pending_hats.set(false);
                        pending_note_off.set(false);
                        pending_silence.set(true);
                    } else {
                        leds.set(0, Led::Button, spectrum_color(glob_genre_fader.get()), LED_BRIGHTNESS);
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
                // Shift held long enough → escalate fill into solo thunderstorm.
                if glob_fill_armed.get() || glob_fill_start.get() || glob_fill_held.get() {
                    glob_fill_solo.set(true);
                }
            } else if !glob_fader_moved.get() {
                // Long: reset to downbeat. Button+fader is the Swing scrub and
                // must not reset.
                glob_reset.set(true);
                glob_fill_armed.set(false);
                glob_fill_start.set(false);
                glob_fill_solo.set(false);
            }
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
                            // Do NOT await params.update here — FRAM + MIDI SysEx
                            // would stall the whole fader/latch task (all layers hang).
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
                    let (feel, density, muted) = storage.query(|s| (s.feel, s.density, s.muted));
                    glob_feel.set(feel);
                    glob_density.set(density);
                    glob_muted.set(muted);
                    glob_fill_armed.set(false);
                    glob_fill_start.set(false);
                    glob_fill_solo.set(false);
                    // Genre lives in params (Configurator); refresh from there.
                    let g = params.query(|p| p.genre.min(NUM_GENRES - 1));
                    glob_genre.set(g);
                    glob_genre_fader.set(genre_fader_center(g, NUM_GENRES));
                    // Jack Mode and swing direction are Config params — keep the
                    // live param values.
                    let (jm, sd) =
                        params.query(|p| (p.jack_mode.min(1) as u8, p.swing_dir == 1));
                    glob_jack_mode.set(jm);
                    glob_reversed.set(sd);
                    if muted {
                        leds.unset(0, Led::Button);
                    } else {
                        leds.set(0, Led::Button, spectrum_color(glob_genre_fader.get()), LED_BRIGHTNESS);
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
        let mut stall_ms: u16 = CLOCK_STALL_MS; // start idle until ticks move
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

            // Live Alt/Third LED previews at 1 ms (not clock-gated).
            // Main + idle: keep genre color on Top so the strip isn't dark.
            match latch_active_layer {
                LatchLayer::Alt => {
                    let fader_now = faders.get_value();
                    let color = spectrum_color(fader_now);
                    let led = split_unsigned_value(fader_now);
                    leds.set(0, Led::Top, color, Brightness::Custom(led[0]));
                    leds.set(0, Led::Bottom, color, Brightness::Custom(led[1]));
                    if !glob_fill_armed.get() {
                        leds.set(0, Led::Button, color, Brightness::High);
                    }
                }
                LatchLayer::Third => {
                    let s = glob_feel.get();
                    leds.set(0, Led::Top, Color::Red, Brightness::Custom((s / 16) as u8));
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
            // bright white while piling hits on (fill or solo), dim while breaking.
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
                    leds.set(0, Led::Button, spectrum_color(glob_genre_fader.get()), LED_BRIGHTNESS);
                }
            }
            fill_led_prev = fill_led;

            // Mid→Low duck on hits (yields to mute / fill / genre scrub).
            let duck = glob_button_duck.get();
            if duck > 0 {
                glob_button_duck.set(duck.saturating_sub(1));
            }
            if !fill_led
                && !glob_muted.get()
                && latch_active_layer != LatchLayer::Alt
            {
                let bright = if duck > 0 {
                    Brightness::Low
                } else {
                    LED_BRIGHTNESS
                };
                leds.set(0, Led::Button, spectrum_color(glob_genre_fader.get()), bright);
            }
        }
    };

    // Persist genre off the fader hot-path. Flush while Shift is held too so
    // Scopepunk / configurator see the nearest genre live (same as Chord Vamp).
    // Scene storage is also flushed here — never modify_and_save from faders/buttons
    // while saver_task may hold the RefCell.
    let genre_persist = async {
        loop {
            app.delay_millis(40).await;
            if glob_genre_dirty.get() {
                glob_genre_dirty.set(false);
                let g = glob_genre.get().min(NUM_GENRES - 1);
                params.update(|p| p.genre = g).await;
            }
            if glob_storage_dirty.get() {
                glob_storage_dirty.set(false);
                storage.modify_and_save(|s| {
                    s.feel = glob_feel.get();
                    s.density = glob_density.get();
                    s.muted = glob_muted.get();
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
