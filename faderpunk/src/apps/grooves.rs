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
    App, AppParams, AppStorage, Die, Led, ManagedStorage, MidiOutput, ParamStore, SceneEvent,
};
use crate::apps::genre_palette::{genre_fader_center, GENRE_NAMES, NUM_GENRES};
use crate::apps::groove::{
    feel_curve, feel_lerp_i32, feel_lerp_u16, humanize_curve, swing_bias, swing_delay_ms,
    FLAT_VEL, SIXTEENTH,
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
/// Seeded tick duration in 1/16-ms units (~20.8 ms ≈ 120 BPM).
const TICK_MS_X16_INIT: u32 = 333;
/// Reject tick measurements longer than this — a clock that was stopped leaves
/// a huge gap that would otherwise poison the tempo estimate for a whole bar.
const TICK_MS_PLAUSIBLE_MAX: u16 = 80;
/// Shortest gate we hand out. The voice engine polls at 1 ms and drains offs
/// before ons, so a sub-poll gate could have its note-off consumed before the
/// note-on was ever sent, leaving the voice stuck on.
const MIN_GATE_MS: u32 = 4;
/// Humanized Feel needed before the ghost window starts walking between bars.
const GHOST_ROTATE_FEEL: u16 = 1024;

/// 16 sixteenths per 4/4 bar.
const STEPS_PER_BAR: u32 = 16;

/// Jack duty and its modifier in one param: the CV Out level shape and the CV
/// In destination are mutually exclusive, so they share a single enum.
const JACK_OUT_ANY: usize = 0;
const JACK_OUT_STACKED: usize = 1;
const JACK_IN_DENSITY: usize = 2;
const JACK_IN_FEEL: usize = 3;
const JACK_IN_RESET: usize = 4;
const JACK_COUNT: usize = 5;

fn jack_is_out(jack: usize) -> bool {
    jack <= JACK_OUT_STACKED
}

/// Voice order — indexes `Params::notes`, the `Ch Map` nibbles and the
/// per-voice engine arrays. Kick/Snare/Hats stay first so the three original
/// params keep their slots.
const V_KICK: usize = 0;
const V_SNARE: usize = 1;
const V_HATS: usize = 2;
const V_OPEN_HAT: usize = 3;
const V_LOW_TOM: usize = 4;
const V_HIGH_TOM: usize = 5;
const V_CLAP: usize = 6;
const VOICES: usize = 7;

/// Note 0 is useless as a drum sound, so it doubles as the "voice off"
/// sentinel. That keeps the four added voices optional without spending a
/// param each on an enable switch — and with all four off, Grooves behaves
/// exactly as it did before they existed.
const VOICE_OFF: u8 = 0;

fn voice_enabled(note: MidiNote) -> bool {
    note != MidiNote::from(VOICE_OFF)
}

/// `Ch Map` packs one channel per voice into a nibble (voice 0 = bits 0..3).
/// A whole map of 0 means "every voice follows the base MIDI Ch", which is the
/// shipped default; any non-zero map is read literally, nibble + 1 = channel.
const CH_MAP_FOLLOW: i32 = 0;
const CH_MAP_MAX: i32 = (1 << (4 * VOICES)) - 1;
/// Keeps the literal bounds in `CONFIG` honest against the packing above.
const _: () = assert!(CH_MAP_FOLLOW == 0 && CH_MAP_MAX == 268_435_455);

/// Stacked CV keeps its three-family voltage vocabulary (hats 1, snare 2,
/// kick 4 units of 10) so existing patches stay calibrated. The extra voices
/// fold into the family they belong to rather than inventing new levels.
fn voice_family_units(voice: usize) -> u16 {
    match voice {
        V_HATS | V_OPEN_HAT => 1,
        V_SNARE | V_CLAP => 2,
        _ => 4,
    }
}

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
    /// Per-16th step character in ms at Feel=max (not swing — odd/even delay
    /// lives in [`swing_delay_ms`]). Typical −6..=+8.
    timing: [i8; 16],
    /// Per-family pocket push in ms at Feel=max: [kick, snare, hats].
    /// Positive = behind the grid. Toms follow kick, clap snare, open hat hats.
    push_ms: [i8; 3],
    /// Humanization scale % (jitter width). Techno tight … Trip-Hop loose.
    tightness: u8,
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
        timing: [0, 2, 0, 3, 2, 3, 0, 4, 0, 2, 0, 3, 2, 3, 0, 4],
        push_ms: [0, 14, 4],
        tightness: 100,
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
        timing: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        push_ms: [0, 2, -4],
        tightness: 70,
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
        timing: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        push_ms: [0, 3, -3],
        tightness: 60,
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
        timing: [0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -1, 0, 0, 0, -2],
        push_ms: [-2, 0, 0],
        tightness: 35,
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
        timing: [0, 2, 4, 6, 3, 4, 5, 7, 0, 2, 4, 6, 6, 8, 5, 7],
        push_ms: [6, 18, 8],
        tightness: 150,
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
        timing: [0, 3, 2, 5, 0, 3, 2, 5, 0, 3, 2, 5, 2, 5, 4, 7],
        push_ms: [2, 12, -2],
        tightness: 130,
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
        timing: [0, 4, -3, 2, 2, 4, 0, 5, 0, 4, -3, 2, 2, 4, 0, 6],
        push_ms: [0, 6, -5],
        tightness: 110,
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
        timing: [0, 5, -4, 3, 0, 5, 2, 6, 0, 5, -4, 3, 0, 5, 2, 6],
        push_ms: [0, 8, -6],
        tightness: 90,
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
        timing: [0, 2, 0, 3, 2, 3, 0, 4, 0, 2, 0, 3, 2, 3, 0, 4],
        push_ms: [0, 10, 2],
        tightness: 80,
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
.add_param(Param::MidiNote {
    name: "MIDI Note Snare",
})
.add_param(Param::MidiNote {
    name: "MIDI Note Hats",
})
.add_param(Param::MidiNote {
    name: "MIDI Note Open Hat",
})
.add_param(Param::MidiNote {
    name: "MIDI Note Low Tom",
})
.add_param(Param::MidiNote {
    name: "MIDI Note High Tom",
})
.add_param(Param::MidiNote {
    name: "MIDI Note Clap",
})
.add_param(Param::MidiChannel { name: "MIDI Ch" })
// Literal bounds: the catalog generator reads these as syntax, and a path
// expression would come out as an enum tag instead of a number.
.add_param(Param::i32 {
    name: "Ch Map",
    min: 0,
    max: 268_435_455,
})
.add_param(Param::Enum {
    name: "Groove",
    variants: GENRE_NAMES,
})
.add_param(Param::i32 {
    name: "Swing max %",
    min: -100,
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
    variants: &[
        "CV Out Any",
        "CV Out Stacked",
        "CV In Density",
        "CV In Feel",
        "CV In Reset",
    ],
})
.add_param(Param::Range {
    name: "Range",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::i32 {
    name: "CV Att",
    min: 0,
    max: 100,
});

pub struct Params {
    /// Indexed by the `V_*` voice constants. Note 0 disables a voice, which is
    /// the default for everything past Hats.
    notes: [MidiNote; VOICES],
    /// Base channel; every voice uses it unless `ch_map` overrides.
    midi_ch: MidiChannel,
    /// Seven packed nibbles, one channel per voice. See `CH_MAP_FOLLOW`.
    ch_map: i32,
    genre: usize,
    /// Swing cap in percent; the sign carries the direction, so a negative
    /// value swings the offbeats early instead of late.
    swing_max_pct: i32,
    gatel: i32,
    midi_out: MidiOut,
    jack: usize,
    range: Range,
    cv_att: i32,
}

impl Params {
    /// Resolve the sending channel for one voice.
    fn channel_for(&self, voice: usize) -> MidiChannel {
        if self.ch_map == CH_MAP_FOLLOW {
            return self.midi_ch;
        }
        let nibble = ((self.ch_map >> (4 * voice)) & 0xF) as u8;
        MidiChannel::from(nibble + 1)
    }
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < PARAMS {
            return None;
        }
        // The pre-voices layout was 16 values too, so length alone can't tell
        // them apart — but it carried the per-voice channel at index 1, where
        // the Snare note now lives. Reject it rather than read channels as
        // notes; `ParamStore` then falls back to the defaults below.
        if matches!(values[1], Value::MidiChannel(_)) {
            return None;
        }
        Some(Self {
            notes: [
                MidiNote::from_value(values[V_KICK]),
                MidiNote::from_value(values[V_SNARE]),
                MidiNote::from_value(values[V_HATS]),
                MidiNote::from_value(values[V_OPEN_HAT]),
                MidiNote::from_value(values[V_LOW_TOM]),
                MidiNote::from_value(values[V_HIGH_TOM]),
                MidiNote::from_value(values[V_CLAP]),
            ],
            midi_ch: MidiChannel::from_value(values[7]),
            ch_map: i32::from_value(values[8]).clamp(CH_MAP_FOLLOW, CH_MAP_MAX),
            genre: usize::from_value(values[9]).min(NUM_GENRES - 1),
            swing_max_pct: i32::from_value(values[10]).clamp(-100, 100),
            gatel: i32::from_value(values[11]),
            midi_out: MidiOut::from_value(values[12]),
            jack: usize::from_value(values[13]).min(JACK_COUNT - 1),
            range: Range::from_value(values[14]),
            cv_att: i32::from_value(values[15]).clamp(0, 100),
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        for note in self.notes.iter() {
            vec.push((*note).into()).unwrap();
        }
        vec.push(self.midi_ch.into()).unwrap();
        vec.push(self.ch_map.into()).unwrap();
        vec.push(self.genre.into()).unwrap();
        vec.push(self.swing_max_pct.into()).unwrap();
        vec.push(self.gatel.into()).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.jack.into()).unwrap();
        vec.push(self.range.into()).unwrap();
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
    muted: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            // Mid-high so a fresh instance already grooves.
            feel: 2800,
            density: 2048,
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
///
/// `rot` walks the revealed window along the fill mask from bar to bar, so a
/// held density doesn't ghost the same steps forever. The caller keeps it at 0
/// while Feel is low — a straight groove has to repeat exactly.
fn fill_reveal(fill: u16, density: u16, step: u32, rot: u32) -> Option<u8> {
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
    let rank = (rank + (rot % total.max(1))) % total.max(1);
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
    let jitter = (u32::from(humanize_curve(feel)) * 12) / 4095;
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
    let open = u32::from(humanize_curve(feel));
    // ~4% at Feel=0 … ~20% at Feel=max.
    let chance = 4 + (open * 16) / 4095;
    u32::from((die.roll() ^ u16::from(salt)) % 100) < chance
}

/// Extra micro-timing push for ghost-only steps (no core hit landing on the
/// same step): as density rises, revealed ghosts drag a little further
/// behind the grid, like a drummer digging into the pocket.
fn ghost_drag_ms(density: u16, feel: u16) -> u32 {
    let raw = (density as u32 * 4) / 4095;
    (raw * u32::from(humanize_curve(feel))) / 4095
}

fn midi_vel(mult: u16) -> u16 {
    // mult is 0..=100 "percent" of full scale
    ((4095u32 * mult as u32) / 100).min(4095) as u16
}

/// Core-hit velocity % from Pattern DNA, attenuated by Feel, plus human jitter
/// scaled by genre tightness. Lateness softening is applied after due-ms is known.
fn core_vel_pct(
    base: u8,
    accent: u8,
    acc_mask: u16,
    step: u32,
    feel: u16,
    die: &Die,
    tightness: u8,
) -> u16 {
    let character = if bit_set(acc_mask, step) {
        u16::from(accent)
    } else {
        u16::from(base)
    };
    let mut v = feel_lerp_u16(FLAT_VEL, character, feel);
    // ±0..=8% jitter × tightness × humanize.
    let span = (u32::from(humanize_curve(feel)) * 8 * u32::from(tightness)) / (4095 * 100);
    if span > 0 {
        let off = u32::from(die.roll()) % (2 * span + 1);
        v = (u32::from(v) + off)
            .saturating_sub(span)
            .clamp(1, 100) as u16;
    }
    v
}

/// Offbeat closed-hat duck: classic loud/soft 16th alternation, Feel-scaled.
fn hat_offbeat_duck(vel: u16, step: u32, accented: bool, feel: u16) -> u16 {
    if accented || step.is_multiple_of(2) {
        return vel;
    }
    let cut = (12u32 * u32::from(humanize_curve(feel))) / 4095;
    vel.saturating_sub(cut as u16).max(1)
}

/// Four-bar phrase arc: bar-of-phrase lift + pickup crescendo into the one.
fn phrase_vel_boost(slot: u32, step: u32) -> i16 {
    let bar = (slot / STEPS_PER_BAR) % 4;
    let lift: i16 = match bar {
        0 => 4,
        1 => 0,
        2 => 2,
        _ => -2,
    };
    let pickup = if bar == 3 && step >= STEPS_PER_BAR - 4 {
        6i16 * (step as i16 - (STEPS_PER_BAR as i16 - 5)) / 4
    } else {
        0
    };
    lift + pickup
}

/// Feel scales the arc away entirely: at Feel = 0 the groove is a machine and
/// must not breathe, or Techno stops being straight.
fn apply_phrase_boost(vel: u16, slot: u32, step: u32, feel: u16) -> u16 {
    let raw = i32::from(phrase_vel_boost(slot, step));
    let scaled = (raw * i32::from(humanize_curve(feel))) / 4095;
    (i32::from(vel) + scaled).clamp(1, 100) as u16
}

/// Effective swing % from bias × Feel, capped by Swing max %.
fn feel_swing_pct(bias: u8, feel: u16, swing_max_pct: i32) -> i32 {
    let f = u32::from(feel_curve(feel));
    let pct = (u32::from(bias) * f) / 4095;
    // Only the magnitude caps the swing — the sign picks the direction.
    pct.min(swing_max_pct.unsigned_abs().min(100)) as i32
}

/// Which `push_ms` slot a voice reads (clap→snare, open hat→hats, toms→kick).
fn voice_push_family(voice: usize) -> usize {
    match voice {
        V_SNARE | V_CLAP => 1,
        V_HATS | V_OPEN_HAT => 2,
        _ => 0,
    }
}

/// Gate length as % of `GATE %` for each voice.
fn gate_factor_pct(voice: usize) -> u32 {
    match voice {
        V_HATS => 45,
        V_OPEN_HAT => 260,
        V_CLAP => 120,
        V_LOW_TOM | V_HIGH_TOM => 150,
        _ => 100,
    }
}

/// How far a voice's note may ring past its own 16th. A drummer's limbs are
/// not chopped at the step boundary, and the voice engine sends note-off
/// before every re-trigger, so the cap only has to stop a voice outliving the
/// groove — not protect the grid. The CV pulse keeps its own, shorter window.
fn gate_span_16ths(voice: usize) -> u32 {
    match voice {
        V_HATS => 1,
        V_OPEN_HAT | V_CLAP | V_LOW_TOM | V_HIGH_TOM => 4,
        _ => 2,
    }
}

/// Half-width of the timing jitter in ms, Feel × tightness scaled. The early
/// budget reserves this much so the jitter stays symmetric instead of being
/// truncated against the grid.
fn jitter_span_ms(feel: u16, tightness: u8) -> i32 {
    ((1 + (u32::from(humanize_curve(feel)) * 5) / 4095) * u32::from(tightness) / 100) as i32
}

/// Per-family share of the jitter width. A drummer's foot is steadier than the
/// hand ornamenting on the hats, so a uniform spread reads as sloppy rather
/// than loose — the kick has to hold the floor the others lean against.
fn jitter_family_pct(voice: usize) -> u32 {
    match voice {
        V_SNARE | V_CLAP => 75,
        V_HATS | V_OPEN_HAT => 100,
        _ => 50,
    }
}

/// Signed humanization jitter in ms, Feel × tightness × family scaled.
fn timing_jitter_ms(die: &Die, feel: u16, tightness: u8, voice: usize) -> i32 {
    let span = (jitter_span_ms(feel, tightness).max(0) as u32 * jitter_family_pct(voice)) / 100;
    if span == 0 {
        return 0;
    }
    let roll = u32::from(die.roll() ^ (voice as u16).wrapping_mul(17)) % (2 * span + 1);
    roll as i32 - span as i32
}

/// How far ahead of the grid this step could need to fire: the earliest family
/// push plus the jitter span. Every voice is delayed by this much, which is the
/// only way to let one voice sit *ahead* of another without a lookahead.
///
/// It is derived rather than constant so it collapses to zero at Feel = 0 —
/// a flat groove then lands on the clock exactly like Bassment / Vamp instead
/// of trailing them. What lateness remains at high Feel is the unavoidable
/// price of the pocket itself.
fn early_budget_ms(pushes: [i32; 3], jitter_span: i32) -> u32 {
    let earliest = pushes.iter().copied().min().unwrap_or(0).min(0);
    ((-earliest) + jitter_span.max(0)) as u32
}

/// Per-voice due delay in ms from slot start, measured from the budget-shifted
/// grid. Clamped so it never reaches the next 16th.
fn voice_due_ms(
    budget: u32,
    swing: u32,
    timing_off: i32,
    push: i32,
    jitter: i32,
    ghost_extra: u32,
    sixteenth_ms: u32,
) -> u32 {
    let raw =
        budget as i32 + swing as i32 + timing_off + push + jitter + ghost_extra as i32;
    let max = sixteenth_ms.saturating_sub(2).max(1) as i32;
    raw.clamp(0, max) as u32
}

/// MIDI note length for one voice.
fn voice_gate_ms(
    voice: usize,
    gate_pct: i32,
    sixteenth_ms: u32,
    accented: bool,
    ghost: bool,
) -> u32 {
    let base = (sixteenth_ms * gate_pct.clamp(1, 100) as u32) / 100;
    let mut len = (base * gate_factor_pct(voice)) / 100;
    if accented {
        len = (len * 125) / 100;
    }
    if ghost {
        len = (len * 70) / 100;
    }
    let cap = sixteenth_ms.saturating_mul(gate_span_16ths(voice));
    len.min(cap).max(MIN_GATE_MS)
}

/// CV pulse length. The jack stays a trigger even when the note it mirrors
/// rings on: a patched Echolot / envelope wants an edge per hit, not a gate
/// that happens to be four 16ths long because Open Hat is switched on.
fn cv_pulse_ms(note_len_ms: u32, room_ms: u32) -> u32 {
    note_len_ms.min(room_ms).max(1)
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

/// Loudest family that fired this step. Open Hat reads as hats, Clap as snare
/// and the toms as kick, so the three original voltages still mean the same
/// thing once the extra voices are switched on.
fn any_pulse_level(hits: &[bool; VOICES], range: Range) -> u16 {
    let mut level = 0u16;
    for (voice, fired) in hits.iter().enumerate() {
        if !fired {
            continue;
        }
        level = level.max(match voice_family_units(voice) {
            1 => 1400,
            2 => 2600,
            _ => 4095,
        });
    }
    pulse_on_range(level, range)
}

/// Binary-weighted family sum: hats 1, snare 2, kick 4 of a nominal 10 units,
/// so each combination keeps its own distinct voltage. Voices double up within
/// a family (Clap under Snare, toms under Kick) rather than adding a unit each,
/// which would push the sum past the 10-unit scale and clip.
fn stacked_pulse_level(hits: &[bool; VOICES], range: Range) -> u16 {
    let mut units = 0u16;
    for family in [1u16, 2, 4] {
        let fired = hits
            .iter()
            .enumerate()
            .any(|(voice, hit)| *hit && voice_family_units(voice) == family);
        if fired {
            units += family;
        }
    }
    let uni = ((units as u32 * 4095) / 10).min(4095) as u16;
    pulse_on_range(uni, range)
}

/// Fold the four optional voices out of the three written ones. Every rule
/// borrows a hit the groove already plays, so no genre needs extra mask data —
/// and with all four voices off this leaves `hits` exactly as it found it.
fn derive_extra_voices(
    hits: &mut [bool; VOICES],
    vels: &mut [u16; VOICES],
    enabled: &[bool; VOICES],
    step: u32,
    gesture: bool,
    snare_accented: bool,
    hats_accented: bool,
) {
    // Toms only appear inside a fill, break or solo, and only in the back half
    // of the bar, where they turn the snare figure into the descending run that
    // makes a fill read as a fill: high tom first, low tom to land it. The
    // snare steps aside, otherwise the run just thickens instead of descending.
    if gesture && hits[V_SNARE] && step >= STEPS_PER_BAR / 2 {
        let tom = if step < 3 * STEPS_PER_BAR / 4 {
            V_HIGH_TOM
        } else {
            V_LOW_TOM
        };
        if enabled[tom] {
            hits[tom] = true;
            hits[V_SNARE] = false;
            vels[tom] = vels[V_SNARE];
        }
    }

    // Open hat takes over an accented hat off the beat. Sounding a closed and
    // an open hat on the same step only smears the transient.
    if enabled[V_OPEN_HAT] && hits[V_HATS] && hats_accented && !step.is_multiple_of(4) {
        hits[V_OPEN_HAT] = true;
        hits[V_HATS] = false;
        vels[V_OPEN_HAT] = (u32::from(vels[V_HATS]) * 11 / 10).min(4095) as u16;
    }

    // Clap thickens the backbeat: it layers under the accented snare instead of
    // replacing it, the way house and disco stack the two.
    if enabled[V_CLAP] && hits[V_SNARE] && snare_accented {
        hits[V_CLAP] = true;
        vels[V_CLAP] = (vels[V_SNARE] * 9) / 10;
    }
}

#[embassy_executor::task(pool_size = 4)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(
        app.app_id,
        app.layout_id,
        Params {
            // GM drum map for the three written voices; the four optional ones
            // ship off (note 0) so a fresh instance sounds as it always did.
            notes: [
                MidiNote::from(36),
                MidiNote::from(38),
                MidiNote::from(42),
                MidiNote::from(VOICE_OFF),
                MidiNote::from(VOICE_OFF),
                MidiNote::from(VOICE_OFF),
                MidiNote::from(VOICE_OFF),
            ],
            midi_ch: MidiChannel::default(),
            ch_map: CH_MAP_FOLLOW,
            genre: 2, // House
            swing_max_pct: 50,
            // Fraction of a 16th (same GATE % convention as Euclid/Turing).
            gatel: 100,
            midi_out: MidiOut([true, false, false]), // USB only — all-ports floods cable
            jack: JACK_OUT_ANY,
            range: Range::_0_10V,
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
    let (midi_out, notes, channels, genre, swing_max_pct, gatel, jack, range, cv_att) = params
        .query(|p| {
            (
                p.midi_out,
                p.notes,
                core::array::from_fn::<MidiChannel, VOICES, _>(|v| p.channel_for(v)),
                p.genre.min(NUM_GENRES - 1),
                p.swing_max_pct.clamp(-100, 100),
                p.gatel,
                p.jack.min(JACK_COUNT - 1),
                p.range,
                att_from_pct(p.cv_att),
            )
        });
    // Which voices are wired up at all; everything past Hats is off by default.
    let enabled: [bool; VOICES] = core::array::from_fn(|v| voice_enabled(notes[v]));

    // Ticker only — never CLOCK_PUBSUB (Grooves+Vamp+Bassment+Contura combo).
    let ticks = app.clock_ticker();
    let faders = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
    let die = app.use_die();
    let midi: [MidiOutput; VOICES] =
        core::array::from_fn(|v| app.use_midi_output(midi_out, channels[v], false));
    let out_jack = if jack_is_out(jack) {
        Some(app.make_out_jack(0, range).await)
    } else {
        None
    };
    let in_jack = if jack_is_out(jack) {
        None
    } else {
        Some(app.make_in_jack(0, Range::_Neg5_5V).await)
    };
    if let Some(ref jack) = out_jack {
        jack.set_value(pulse_idle(range));
    }

    let (feel, density, muted) = storage.query(|s| (s.feel, s.density, s.muted));

    let glob_feel = app.make_global(feel);
    let glob_swing_max = app.make_global(swing_max_pct);
    let glob_density = app.make_global(density);
    // Jack shape and swing direction are Config params — scenes no longer own
    // them. The swing sign is the direction, so both ride on one value.
    let glob_stacked = app.make_global(jack == JACK_OUT_STACKED);
    let glob_reversed = app.make_global(swing_max_pct < 0);
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
    // Per-voice note-offs from independent gate lengths (open hat may overhang).
    let pending_offs = app.make_global([false; VOICES]);
    // Clock watch → voice engine: one flag and one velocity per voice. The clock
    // side only fills these in, and `fut_voice` is the sole place that talks to
    // MIDI — sending from the clock subscriber stalls the device clock.
    let pending_hits = app.make_global([false; VOICES]);
    let pending_vels = app.make_global([0u16; VOICES]);

    // Clear any hanging notes from a prior respawn.
    for voice in 0..VOICES {
        if enabled[voice] {
            midi[voice].send_note_off(notes[voice]).await;
        }
    }

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
        // Per-voice schedule for the current 16th (u32::MAX = idle).
        let mut target_ms = [u32::MAX; VOICES];
        let mut sched_vel = [0u16; VOICES];
        let mut sched_midi = [false; VOICES];
        let mut sched_cv = [false; VOICES];
        let mut sched_acc = [false; VOICES];
        let mut sched_gh = [false; VOICES];
        let mut fired = [false; VOICES];
        let mut gate_off_wall: [Option<u32>; VOICES] = [None; VOICES];
        // CV runs its own, shorter window so the jack stays a trigger.
        let mut cv_off_wall: [Option<u32>; VOICES] = [None; VOICES];
        let mut cv_on = [false; VOICES];
        let mut scheduled_slot = u32::MAX;
        let mut wall_ms: u32 = 0;
        let mut ms_in_tick: u16 = 0;
        let mut tick_ms_x16: u32 = TICK_MS_X16_INIT;

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
            wall_ms = wall_ms.wrapping_add(1);

            let t = ticks();
            let mut do_stop = false;
            if t == last_tick {
                stall_ms = stall_ms.saturating_add(1);
                ms_in_tick = ms_in_tick.saturating_add(1);
                if stall_ms == 250 {
                    do_stop = true;
                }
                // Fall through: ms-resolution dues/gates need every poll.
            } else if t < last_tick {
                do_stop = true;
                last_tick = t;
                stall_ms = 0;
                ms_in_tick = 0;
            } else {
                // Only single-tick gaps of a plausible length say anything about
                // the tempo. The first tick after a stopped clock carries the
                // whole idle time and would otherwise drag the estimate to the
                // clamp ceiling for a bar's worth of ticks.
                if t == last_tick.wrapping_add(1)
                    && ms_in_tick > 0
                    && ms_in_tick <= TICK_MS_PLAUSIBLE_MAX
                {
                    tick_ms_x16 = (tick_ms_x16 * 3 + (ms_in_tick as u32) * 16) / 4;
                    // Keep tempo estimate in a sane band (~40–300 BPM).
                    tick_ms_x16 = tick_ms_x16.clamp(80, 1000);
                }
                ms_in_tick = 0;
                stall_ms = 0;
                last_tick = t;
            }

            if do_stop {
                pending_hits.set([false; VOICES]);
                pending_offs.set([false; VOICES]);
                pending_note_off.set(false);
                pending_silence.set(true);
                if let Some(ref jack) = out_jack {
                    jack.set_value(pulse_idle(range));
                }
                target_ms = [u32::MAX; VOICES];
                fired = [false; VOICES];
                gate_off_wall = [None; VOICES];
                cv_off_wall = [None; VOICES];
                cv_on = [false; VOICES];
                scheduled_slot = u32::MAX;
                origin_set = false;
                ms_in_tick = 0;
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

            // Mute abandons any ringing CV window outright. Without this the
            // recompute below would find a voice still "on" and pull the jack
            // back up after the button task had already parked it.
            if glob_muted.get() && cv_on.iter().any(|on| *on) {
                cv_on = [false; VOICES];
                cv_off_wall = [None; VOICES];
                if let Some(ref jack) = out_jack {
                    jack.set_value(pulse_idle(range));
                }
                leds.set(
                    0,
                    Led::Bottom,
                    spectrum_color(glob_genre_fader.get()),
                    Brightness::Off,
                );
            }

            // Per-voice note and CV offs (wall clock) — independent lengths.
            {
                let mut offs = pending_offs.get();
                let mut any_off = false;
                let mut cv_changed = false;
                for voice in 0..VOICES {
                    if let Some(off_at) = gate_off_wall[voice] {
                        if wall_ms >= off_at {
                            gate_off_wall[voice] = None;
                            offs[voice] = true;
                            any_off = true;
                        }
                    }
                    if let Some(off_at) = cv_off_wall[voice] {
                        if wall_ms >= off_at {
                            cv_off_wall[voice] = None;
                            cv_on[voice] = false;
                            cv_changed = true;
                        }
                    }
                }
                if any_off {
                    pending_offs.set(offs);
                }
                if cv_changed {
                    // Recompute from what is still pulsing — dropping straight to
                    // idle would misreport the remaining voices, and in Stacked
                    // mode the level *is* the information.
                    let still_on = cv_on.iter().any(|on| *on);
                    if let Some(ref jack) = out_jack {
                        jack.set_value(if still_on {
                            if glob_stacked.get() {
                                stacked_pulse_level(&cv_on, range)
                            } else {
                                any_pulse_level(&cv_on, range)
                            }
                        } else {
                            pulse_idle(range)
                        });
                    }
                    if !still_on {
                        leds.set(
                            0,
                            Led::Bottom,
                            spectrum_color(glob_genre_fader.get()),
                            Brightness::Off,
                        );
                    }
                }
            }

            let clkn = ticks() as u32;

            if !origin_set || glob_reset.get() {
                origin = clkn;
                origin_set = true;
                scheduled_slot = u32::MAX;
                target_ms = [u32::MAX; VOICES];
                fired = [false; VOICES];
                glob_reset.set(false);
                glob_fill_armed.set(false);
                glob_fill_start.set(false);
                glob_fill_solo.set(false);
                fill_start_slot = 0;
            }

            let pos = clkn.wrapping_sub(origin);
            let slot = pos / SIXTEENTH;
            let step = (pos / SIXTEENTH) % STEPS_PER_BAR;
            let phase = pos % SIXTEENTH;
            let sixteenth_ms = ((SIXTEENTH * tick_ms_x16) / 16).max(8);
            let elapsed_ms =
                (phase * tick_ms_x16 / 16).saturating_add(ms_in_tick as u32);

            let feel_val = if jack == JACK_IN_FEEL {
                mod_u16(glob_feel.get(), glob_cv_val.get())
            } else {
                glob_feel.get()
            };
            let (g_lo, g_hi, g_frac) = genre_pair(glob_genre_fader.get(), NUM_GENRES);
            let near = genre_nearest(glob_genre_fader.get(), NUM_GENRES);
            let pat = &PATTERNS[near];
            let pat_lo = &PATTERNS[g_lo];
            let pat_hi = &PATTERNS[g_hi];

            let density = if jack == JACK_IN_DENSITY {
                mod_u16(glob_density.get(), glob_cv_val.get())
            } else {
                glob_density.get()
            };

            if glob_fill_start.get() {
                glob_fill_start.set(false);
                glob_fill_variant.set(fill_variant(&die, near, feel_val));
                glob_fill_break.set(density >= BREAK_DENSITY);
                glob_fill_solo.set(false);
                glob_fill_armed.set(true);
                fill_start_slot = slot;
            } else if glob_fill_armed.get() && step == 0 && slot > fill_start_slot {
                if glob_fill_held.get() {
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
            let swing =
                swing_delay_ms(step, swing_pct, glob_reversed.get(), sixteenth_ms);
            let tightness = lerp_u8(pat_lo.tightness, pat_hi.tightness, g_frac);
            // Ghost window walks on a four-bar phrase, and only once Feel is
            // past the flat zone — below that the bar has to repeat verbatim.
            let ghost_rot = if humanize_curve(feel_val) >= GHOST_ROTATE_FEEL {
                (slot / STEPS_PER_BAR) % 4
            } else {
                0
            };

            // Schedule once per 16th slot (ms dues released below).
            if slot != scheduled_slot && !glob_muted.get() {
                scheduled_slot = slot;
                target_ms = [u32::MAX; VOICES];
                sched_vel = [0; VOICES];
                sched_midi = [false; VOICES];
                sched_cv = [false; VOICES];
                sched_acc = [false; VOICES];
                sched_gh = [false; VOICES];
                fired = [false; VOICES];

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

                let (kick_ghost, snare_ghost, hats_ghost) = if gesture {
                    (None, None, None)
                } else {
                    (
                        fill_reveal(pat.kick_fill, density, step, ghost_rot),
                        fill_reveal(pat.snare_fill, density, step, ghost_rot),
                        fill_reveal(pat.hats_fill, density, step, ghost_rot),
                    )
                };

                let do_kick = kick_core || kick_ghost.is_some();
                let do_snare = snare_core || snare_ghost.is_some();
                let do_hats = hats_core || hats_ghost.is_some();
                let core_hit = kick_core || snare_core || hats_core;

                let mut hits = [false; VOICES];
                let mut vels = [0u16; VOICES];
                let mut ghosts = [false; VOICES];

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
                                &die,
                                tightness,
                            ),
                        }
                    };
                    vels[V_KICK] = apply_phrase_boost(v, slot, step, feel_val);
                    hits[V_KICK] = true;
                    ghosts[V_KICK] = kick_gh || (kick_ghost.is_some() && !kick_core);
                    sched_acc[V_KICK] = if solo_fig.is_some() {
                        kick_acc_hit
                    } else {
                        bit_set(pat.kick_acc_mask, step)
                    };
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
                                lerp_u8(pat_lo.snare_accent, pat_hi.snare_accent, g_frac),
                                pat.snare_acc_mask,
                                step,
                                feel_val,
                                &die,
                                tightness,
                            ),
                        }
                    };
                    vels[V_SNARE] = apply_phrase_boost(v, slot, step, feel_val);
                    hits[V_SNARE] = true;
                    ghosts[V_SNARE] = snare_gh || (snare_ghost.is_some() && !snare_core);
                    sched_acc[V_SNARE] = if solo_fig.is_some() {
                        snare_acc_hit
                    } else {
                        bit_set(pat.snare_acc_mask, step)
                    };
                }
                if do_hats {
                    let hats_accented = if solo_fig.is_some() {
                        hats_acc_hit
                    } else {
                        bit_set(pat.hats_acc_mask, step)
                    };
                    let mut v = if solo_fig.is_some() {
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
                                &die,
                                tightness,
                            ),
                        }
                    };
                    if !gesture {
                        v = hat_offbeat_duck(v, step, hats_accented, feel_val);
                    }
                    vels[V_HATS] = apply_phrase_boost(v, slot, step, feel_val);
                    hits[V_HATS] = true;
                    ghosts[V_HATS] = hats_gh || (hats_ghost.is_some() && !hats_core);
                    sched_acc[V_HATS] = hats_accented;
                }

                let snare_accented = sched_acc[V_SNARE];
                let hats_accented = sched_acc[V_HATS];
                // Percent → MIDI velocity domain before derive (open hat / clap
                // scale 0..=4095 values, same as the pre-feel engine).
                for voice in [V_KICK, V_SNARE, V_HATS] {
                    if hits[voice] {
                        vels[voice] = midi_vel(vels[voice]);
                    }
                }
                derive_extra_voices(
                    &mut hits,
                    &mut vels,
                    &enabled,
                    step,
                    gesture,
                    snare_accented,
                    hats_accented,
                );
                for voice in 0..VOICES {
                    if !hits[voice] || voice <= V_HATS {
                        continue;
                    }
                    ghosts[voice] = match voice {
                        V_CLAP => ghosts[V_SNARE],
                        V_OPEN_HAT => ghosts[V_HATS],
                        _ => ghosts[V_SNARE],
                    };
                    sched_acc[voice] = match voice {
                        V_CLAP => snare_accented,
                        V_OPEN_HAT => hats_accented,
                        _ => sched_acc[V_SNARE],
                    };
                }

                let ghost_extra_base = if !core_hit
                    && (kick_ghost.is_some() || snare_ghost.is_some() || hats_ghost.is_some())
                {
                    ghost_drag_ms(density, feel_val)
                } else {
                    0
                };

                let pushes: [i32; 3] = core::array::from_fn(|fam| {
                    let push_char = lerp_i32(
                        i32::from(pat_lo.push_ms[fam]),
                        i32::from(pat_hi.push_ms[fam]),
                        g_frac,
                    );
                    feel_lerp_i32(0, push_char, feel_val)
                });
                let budget = early_budget_ms(pushes, jitter_span_ms(feel_val, tightness));

                for voice in 0..VOICES {
                    if !hits[voice] {
                        continue;
                    }
                    let push = pushes[voice_push_family(voice)];
                    let jitter =
                        timing_jitter_ms(&die, feel_val, tightness, voice);
                    let g_extra = if ghosts[voice] && !core_hit {
                        ghost_extra_base
                    } else {
                        0
                    };
                    let due = voice_due_ms(
                        budget,
                        swing,
                        timing_off,
                        push,
                        jitter,
                        g_extra,
                        sixteenth_ms,
                    );
                    // Late-behind-budget softens written (non-ghost) hits.
                    let late_ms = due.saturating_sub(budget + swing);
                    if !gesture && !ghosts[voice] && late_ms > 0 {
                        let cut = (late_ms / 2).min(6);
                        let pct = ((u32::from(vels[voice]) * 100) / 4095)
                            .saturating_sub(cut)
                            .max(1);
                        vels[voice] = midi_vel(pct as u16);
                    }
                    target_ms[voice] = due;
                    sched_vel[voice] = vels[voice];
                    sched_cv[voice] = true;
                    sched_midi[voice] = enabled[voice];
                    sched_gh[voice] = ghosts[voice];
                }
            }

            // Release dues whose elapsed time has arrived.
            if !glob_muted.get() && scheduled_slot == slot {
                let mut fire = [false; VOICES];
                let mut fire_vels = [0u16; VOICES];
                let mut any_fire = false;
                let mut cv_hits = [false; VOICES];
                for voice in 0..VOICES {
                    if fired[voice] || target_ms[voice] == u32::MAX {
                        continue;
                    }
                    if elapsed_ms >= target_ms[voice] {
                        fired[voice] = true;
                        let room = sixteenth_ms.saturating_sub(elapsed_ms).max(1);
                        let glen = voice_gate_ms(
                            voice,
                            gatel,
                            sixteenth_ms,
                            sched_acc[voice],
                            sched_gh[voice],
                        );
                        gate_off_wall[voice] = Some(wall_ms.wrapping_add(glen));
                        if sched_cv[voice] {
                            cv_off_wall[voice] =
                                Some(wall_ms.wrapping_add(cv_pulse_ms(glen, room)));
                            cv_on[voice] = true;
                            cv_hits[voice] = true;
                        }
                        if sched_midi[voice] {
                            fire[voice] = true;
                            fire_vels[voice] = sched_vel[voice];
                            any_fire = true;
                        }
                    }
                }
                if cv_hits.iter().any(|h| *h) {
                    let level = if glob_stacked.get() {
                        // Jack reflects currently-on CV voices, not only this poll.
                        stacked_pulse_level(&cv_on, range)
                    } else {
                        any_pulse_level(&cv_on, range)
                    };
                    if let Some(ref jack) = out_jack {
                        jack.set_value(level);
                    }
                    leds.set(
                        0,
                        Led::Bottom,
                        spectrum_color(glob_genre_fader.get()),
                        Brightness::High,
                    );
                    glob_button_duck.set(BUTTON_DUCK_MS);
                }
                if any_fire {
                    // Merge with any still-pending hits from a prior poll.
                    let mut pending = pending_hits.get();
                    let mut pvels = pending_vels.get();
                    for voice in 0..VOICES {
                        if fire[voice] {
                            pending[voice] = true;
                            pvels[voice] = fire_vels[voice];
                        }
                    }
                    pending_vels.set(pvels);
                    pending_hits.set(pending);
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

    // MIDI voice engine — isolated from the clock subscriber so APP_MIDI_CHANNEL
    // backpressure (Harmonica chord storms, Note Fader spam) cannot stall ticks.
    let fut_voice = async {
        let mut sounding = [false; VOICES];
        loop {
            app.delay_millis(1).await;

            let silence = pending_silence.get();
            if silence {
                pending_silence.set(false);
                pending_hits.set([false; VOICES]);
                pending_offs.set([false; VOICES]);
            }

            let offs = pending_offs.get();
            let all_off = silence || pending_note_off.get();
            if all_off || offs.iter().any(|o| *o) {
                pending_note_off.set(false);
                pending_offs.set([false; VOICES]);
                for voice in 0..VOICES {
                    if sounding[voice] && (all_off || offs[voice]) {
                        midi[voice].try_send_note_off(notes[voice]);
                        sounding[voice] = false;
                    }
                }
            }
            if silence {
                continue;
            }

            let fire = pending_hits.get();
            if fire.iter().any(|hit| *hit) {
                pending_hits.set([false; VOICES]);
                if !glob_muted.get() {
                    let vels = pending_vels.get();
                    for voice in 0..VOICES {
                        if !fire[voice] {
                            continue;
                        }
                        if sounding[voice] {
                            midi[voice].try_send_note_off(notes[voice]);
                        }
                        midi[voice].try_send_note_on(notes[voice], vels[voice]);
                        sounding[voice] = true;
                    }
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
                        pending_hits.set([false; VOICES]);
                        pending_offs.set([false; VOICES]);
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
                    // Jack shape and swing direction are Config params — keep the
                    // live param values.
                    let (stacked, reversed) = params
                        .query(|p| (p.jack == JACK_OUT_STACKED, p.swing_max_pct < 0));
                    glob_stacked.set(stacked);
                    glob_reversed.set(reversed);
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
                if jack == JACK_IN_RESET {
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
