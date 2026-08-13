//! Shared no_std ornament timing for Grooves, Bassment, and Contura.
//!
//! Genre articulation profiles feed drum/bass apps; Contura builds plans from
//! scale feel only. Execution (MIDI, CV, note-offs) stays app-local.

use super::genre_palette::NUM_GENRES;
use super::groove::{feel_curve, step_chance, STEPS_PER_BAR};

/// Maximum strikes per parent step (main + two subs).
pub const MAX_HITS: usize = 3;

/// Permille of a parent step (0 = step downbeat, 1000 = next step).
pub const STEP_PERMILLE: u16 = 1000;

/// Density fader below this keeps groove ornaments off (ghosts unchanged).
pub const DENSITY_ORNAMENT_FLOOR: u16 = 2400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OrnamentKind {
    Double = 0,
    Flam = 1,
    Triplet = 2,
    Roll = 3,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OrnamentHit {
    /// Position within the parent step (permille).
    pub offset_permille: u16,
    /// Velocity as % of the parent strike (0–100).
    pub vel_pct: u8,
    /// Gate length as % of the parent gate (0–100).
    pub gate_pct: u8,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OrnamentPlan {
    pub len: u8,
    pub hits: [OrnamentHit; MAX_HITS],
}

impl OrnamentPlan {
    pub const fn empty() -> Self {
        Self {
            len: 0,
            hits: [OrnamentHit {
                offset_permille: 0,
                vel_pct: 0,
                gate_pct: 0,
            }; MAX_HITS],
        }
    }

    pub const fn single_main() -> Self {
        Self {
            len: 1,
            hits: [
                OrnamentHit {
                    offset_permille: 0,
                    vel_pct: 100,
                    gate_pct: 100,
                },
                OrnamentHit {
                    offset_permille: 0,
                    vel_pct: 0,
                    gate_pct: 0,
                },
                OrnamentHit {
                    offset_permille: 0,
                    vel_pct: 0,
                    gate_pct: 0,
                },
            ],
        }
    }
}

/// Relative weights for picking an ornament kind (0–100 each).
#[derive(Clone, Copy, Debug, Default)]
pub struct KindWeights {
    pub double: u8,
    pub flam: u8,
    pub triplet: u8,
    pub roll: u8,
}

/// Per-voice family weights inside a genre profile.
#[derive(Clone, Copy, Debug, Default)]
pub struct GenreVoiceWeights {
    pub kick: KindWeights,
    pub snare: KindWeights,
    pub hats: KindWeights,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrooveVoice {
    Kick,
    Snare,
    Hats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArticContext {
    Groove {
        density: u16,
        feel: u16,
    },
    Fill {
        step: u32,
        feel: u16,
    },
    Solo {
        tier: u8,
        phrase_bar: u32,
        feel: u16,
    },
}

const KIND_PLANS: [OrnamentPlan; 4] = [
    // Double — main + echo
    OrnamentPlan {
        len: 2,
        hits: [
            OrnamentHit {
                offset_permille: 0,
                vel_pct: 100,
                gate_pct: 100,
            },
            OrnamentHit {
                offset_permille: 520,
                vel_pct: 58,
                gate_pct: 65,
            },
            OrnamentHit {
                offset_permille: 0,
                vel_pct: 0,
                gate_pct: 0,
            },
        ],
    },
    // Flam — grace then main
    OrnamentPlan {
        len: 2,
        hits: [
            OrnamentHit {
                offset_permille: 140,
                vel_pct: 38,
                gate_pct: 45,
            },
            OrnamentHit {
                offset_permille: 420,
                vel_pct: 100,
                gate_pct: 100,
            },
            OrnamentHit {
                offset_permille: 0,
                vel_pct: 0,
                gate_pct: 0,
            },
        ],
    },
    // Triplet — three even strokes
    OrnamentPlan {
        len: 3,
        hits: [
            OrnamentHit {
                offset_permille: 0,
                vel_pct: 92,
                gate_pct: 78,
            },
            OrnamentHit {
                offset_permille: 333,
                vel_pct: 82,
                gate_pct: 72,
            },
            OrnamentHit {
                offset_permille: 666,
                vel_pct: 96,
                gate_pct: 80,
            },
        ],
    },
    // Roll — two grace taps into main
    OrnamentPlan {
        len: 3,
        hits: [
            OrnamentHit {
                offset_permille: 180,
                vel_pct: 42,
                gate_pct: 38,
            },
            OrnamentHit {
                offset_permille: 320,
                vel_pct: 48,
                gate_pct: 42,
            },
            OrnamentHit {
                offset_permille: 480,
                vel_pct: 100,
                gate_pct: 100,
            },
        ],
    },
];

/// Nine genre articulation profiles (kick conservative, snare/hats busier).
const GENRE_WEIGHTS: [GenreVoiceWeights; NUM_GENRES] = [
    // Dub
    GenreVoiceWeights {
        kick: KindWeights {
            double: 8,
            flam: 4,
            triplet: 2,
            roll: 2,
        },
        snare: KindWeights {
            double: 14,
            flam: 18,
            triplet: 6,
            roll: 8,
        },
        hats: KindWeights {
            double: 22,
            flam: 12,
            triplet: 10,
            roll: 14,
        },
    },
    // Disco
    GenreVoiceWeights {
        kick: KindWeights {
            double: 12,
            flam: 6,
            triplet: 8,
            roll: 4,
        },
        snare: KindWeights {
            double: 20,
            flam: 14,
            triplet: 16,
            roll: 10,
        },
        hats: KindWeights {
            double: 28,
            flam: 10,
            triplet: 22,
            roll: 16,
        },
    },
    // House
    GenreVoiceWeights {
        kick: KindWeights {
            double: 10,
            flam: 8,
            triplet: 10,
            roll: 6,
        },
        snare: KindWeights {
            double: 18,
            flam: 16,
            triplet: 18,
            roll: 12,
        },
        hats: KindWeights {
            double: 26,
            flam: 14,
            triplet: 28,
            roll: 18,
        },
    },
    // Techno
    GenreVoiceWeights {
        kick: KindWeights {
            double: 6,
            flam: 4,
            triplet: 4,
            roll: 8,
        },
        snare: KindWeights {
            double: 12,
            flam: 10,
            triplet: 8,
            roll: 14,
        },
        hats: KindWeights {
            double: 20,
            flam: 8,
            triplet: 12,
            roll: 22,
        },
    },
    // Trip-Hop
    GenreVoiceWeights {
        kick: KindWeights {
            double: 10,
            flam: 12,
            triplet: 4,
            roll: 6,
        },
        snare: KindWeights {
            double: 16,
            flam: 22,
            triplet: 8,
            roll: 10,
        },
        hats: KindWeights {
            double: 18,
            flam: 16,
            triplet: 10,
            roll: 12,
        },
    },
    // Hip-Hop
    GenreVoiceWeights {
        kick: KindWeights {
            double: 14,
            flam: 10,
            triplet: 6,
            roll: 8,
        },
        snare: KindWeights {
            double: 22,
            flam: 20,
            triplet: 10,
            roll: 14,
        },
        hats: KindWeights {
            double: 24,
            flam: 12,
            triplet: 14,
            roll: 16,
        },
    },
    // Jungle
    GenreVoiceWeights {
        kick: KindWeights {
            double: 8,
            flam: 6,
            triplet: 14,
            roll: 12,
        },
        snare: KindWeights {
            double: 18,
            flam: 14,
            triplet: 28,
            roll: 20,
        },
        hats: KindWeights {
            double: 30,
            flam: 10,
            triplet: 32,
            roll: 24,
        },
    },
    // UK Garage
    GenreVoiceWeights {
        kick: KindWeights {
            double: 10,
            flam: 8,
            triplet: 16,
            roll: 10,
        },
        snare: KindWeights {
            double: 20,
            flam: 16,
            triplet: 24,
            roll: 18,
        },
        hats: KindWeights {
            double: 32,
            flam: 12,
            triplet: 30,
            roll: 20,
        },
    },
    // Dubstep
    GenreVoiceWeights {
        kick: KindWeights {
            double: 12,
            flam: 6,
            triplet: 8,
            roll: 14,
        },
        snare: KindWeights {
            double: 16,
            flam: 12,
            triplet: 12,
            roll: 18,
        },
        hats: KindWeights {
            double: 22,
            flam: 10,
            triplet: 16,
            roll: 24,
        },
    },
];

#[inline]
pub fn plan_for_kind(kind: OrnamentKind) -> OrnamentPlan {
    KIND_PLANS[kind as usize]
}

#[inline]
pub fn genre_weights(genre: usize) -> &'static GenreVoiceWeights {
    &GENRE_WEIGHTS[genre.min(NUM_GENRES - 1)]
}

#[inline]
pub fn voice_weights(profile: &GenreVoiceWeights, voice: GrooveVoice) -> KindWeights {
    match voice {
        GrooveVoice::Kick => profile.kick,
        GrooveVoice::Snare => profile.snare,
        GrooveVoice::Hats => profile.hats,
    }
}

/// Scale a 0..=100 arc by curved Feel (0 when feel is zero).
fn feel_scaled_intensity(base: u32, feel: u16) -> u8 {
    if feel == 0 {
        return 0;
    }
    (base * u32::from(feel_curve(feel)) / 4095).min(100) as u8
}

/// Contextual intensity 0..=100 for ornament rolls.
pub fn artic_intensity(ctx: ArticContext) -> u8 {
    match ctx {
        ArticContext::Groove { density, feel } => {
            if feel == 0 || density < DENSITY_ORNAMENT_FLOOR {
                return 0;
            }
            let d = ((density.saturating_sub(DENSITY_ORNAMENT_FLOOR)) as u32 * 100)
                / (4095 - DENSITY_ORNAMENT_FLOOR) as u32;
            let f = u32::from(feel_curve(feel)) * 100 / 4095;
            (d * f / 100).min(100) as u8
        }
        ArticContext::Fill { step, feel } => {
            let tail = step % STEPS_PER_BAR;
            // Crescendo into the bar line — scaled by Feel.
            let arc = if tail >= 12 {
                90u32
            } else if tail >= 8 {
                65
            } else {
                40
            };
            feel_scaled_intensity(arc, feel)
        }
        ArticContext::Solo {
            tier,
            phrase_bar,
            feel,
        } => {
            let tier_boost = match tier {
                0 => 35u32,
                1 => 55,
                2 => 75,
                _ => 50,
            };
            let arc = match phrase_bar % 4 {
                0 => 20u32,
                1 => 40,
                2 => 60,
                _ => 30,
            };
            feel_scaled_intensity((tier_boost + arc).min(100), feel)
        }
    }
}

fn kind_weights_total(w: KindWeights) -> u16 {
    u16::from(w.double) + u16::from(w.flam) + u16::from(w.triplet) + u16::from(w.roll)
}

/// Pick kind with full proportional weights (entry gate is separate).
fn pick_kind_proportional(weights: KindWeights, roll: u8) -> Option<OrnamentKind> {
    let total = kind_weights_total(weights);
    if total == 0 {
        return None;
    }
    let pick = u16::from(roll) % total;
    let mut acc = 0u16;
    for (w, kind) in [
        (weights.double, OrnamentKind::Double),
        (weights.flam, OrnamentKind::Flam),
        (weights.triplet, OrnamentKind::Triplet),
        (weights.roll, OrnamentKind::Roll),
    ] {
        acc += u16::from(w);
        if pick < acc {
            return Some(kind);
        }
    }
    None
}

/// Sparse entry rate 0..=100 from voice, weights and context intensity.
fn entry_rate_pct(voice: GrooveVoice, weights: KindWeights, intensity: u8, rate_scale: u8) -> u8 {
    if intensity == 0 {
        return 0;
    }
    let voice_mul = match voice {
        GrooveVoice::Kick => 1u32,
        GrooveVoice::Snare => 2,
        GrooveVoice::Hats => 3,
    };
    let cap = match voice {
        GrooveVoice::Kick => 14u8,
        GrooveVoice::Snare => 22,
        GrooveVoice::Hats => 36,
    };
    let sum = u32::from(kind_weights_total(weights));
    let raw =
        sum * voice_mul * u32::from(intensity) * u32::from(rate_scale.max(1)) / (100 * 9 * 100);
    raw.min(u32::from(cap)) as u8
}

/// Bassment persona scales ornament entry (100 = neutral).
pub fn bass_voice_ornament_scale(ghost_pct: u8, syncop_bias: u16, pocket: bool) -> u8 {
    if pocket {
        return 35;
    }
    let g = u32::from(ghost_pct);
    let s = u32::from(syncop_bias.min(1600));
    (70 + g / 3 + s / 50).min(155) as u8
}

/// Fit plan permilles into 0..=STEP_PERMILLE; main at anchor only when the whole plan fits.
fn fit_plan_permilles(plan: &OrnamentPlan, anchor_perm: u16) -> [u16; MAX_HITS] {
    let len = plan.len as usize;
    let mut out = [0u16; MAX_HITS];
    if len == 0 {
        return out;
    }
    let main_i = main_hit_index(plan);
    let main_perm = plan.hits[main_i].offset_permille.min(STEP_PERMILLE);
    let shift = anchor_perm as i32 - main_perm as i32;

    let mut min_ideal = i32::MAX;
    let mut max_ideal = i32::MIN;
    for hit in plan.hits.iter().take(len) {
        let ideal = hit.offset_permille as i32 + shift;
        min_ideal = min_ideal.min(ideal);
        max_ideal = max_ideal.max(ideal);
    }

    let mut adj_shift = shift;
    if min_ideal < 0 {
        adj_shift -= min_ideal;
    }
    max_ideal = i32::MIN;
    for hit in plan.hits.iter().take(len) {
        let ideal = hit.offset_permille as i32 + adj_shift;
        max_ideal = max_ideal.max(ideal);
    }
    if max_ideal > STEP_PERMILLE as i32 {
        adj_shift -= max_ideal - STEP_PERMILLE as i32;
    }

    for (i, hit) in plan.hits.iter().enumerate().take(len) {
        out[i] = (hit.offset_permille as i32 + adj_shift).clamp(0, STEP_PERMILLE as i32) as u16;
    }
    for i in 1..len {
        if out[i] <= out[i - 1] {
            out[i] = (out[i - 1] + 1).min(STEP_PERMILLE);
        }
    }
    out
}

/// Quantize permilles to parent units; enforce strictly ascending dues within the step.
fn quantize_dues(
    perms: &[u16; MAX_HITS],
    len: usize,
    parent: u32,
    min_step: u32,
) -> [u32; MAX_HITS] {
    let mut out = [u32::MAX; MAX_HITS];
    if len == 0 || parent == 0 {
        return out;
    }
    let cap = parent.saturating_sub(1).max(1);
    let step = min_step.max(1);

    for i in 0..len {
        let raw = (parent as u64 * perms[i] as u64 / 1000) as u32;
        out[i] = raw.min(cap);
    }

    // Forward: enforce minimum spacing.
    for i in 1..len {
        if out[i] <= out[i - 1] {
            out[i] = (out[i - 1] + step).min(cap);
        }
    }

    // Backward: when several hits quantize to `cap`, pull earlier ones left.
    for i in (1..len).rev() {
        let max_prev = out[i].saturating_sub(step);
        if out[i - 1] > max_prev {
            out[i - 1] = max_prev;
        }
    }

    // Final forward pass restores strict ascent after backward compression.
    for i in 1..len {
        if out[i] <= out[i - 1] {
            out[i] = (out[i - 1] + step).min(cap);
        }
    }

    // Last resort when `parent >= len` still leaves a tie at the ceiling.
    if len >= 2 && out[len - 1] <= out[len - 2] {
        for i in (1..len).rev() {
            if out[i] <= out[i - 1] {
                out[i - 1] = out[i].saturating_sub(step);
            }
        }
    }

    out
}

fn map_plan_dues(anchor: u32, plan: &OrnamentPlan, parent: u32, min_step: u32) -> [u32; MAX_HITS] {
    if plan.len == 0 || parent == 0 {
        return [u32::MAX; MAX_HITS];
    }
    let anchor_perm = ((anchor as u64 * 1000) / parent as u64).min(1000) as u16;
    let perms = fit_plan_permilles(plan, anchor_perm);
    quantize_dues(&perms, plan.len as usize, parent, min_step)
}

/// Genre-driven ornament plan for Grooves / Bassment. Returns single main when off.
/// `rate_scale`: 100 = neutral; Bassment passes persona-scaled values.
pub fn groove_plan(
    genre: usize,
    voice: GrooveVoice,
    ctx: ArticContext,
    chance: u8,
    salt: u32,
    rate_scale: u8,
) -> OrnamentPlan {
    let intensity = artic_intensity(ctx);
    if intensity == 0 {
        return OrnamentPlan::single_main();
    }
    let weights = voice_weights(genre_weights(genre), voice);
    let entry = entry_rate_pct(voice, weights, intensity, rate_scale);
    if entry == 0 || chance >= entry {
        return OrnamentPlan::single_main();
    }
    let sub = step_chance(chance as u32, voice as usize, salt);
    match pick_kind_proportional(weights, sub) {
        Some(kind) => plan_for_kind(kind),
        None => OrnamentPlan::single_main(),
    }
}

/// Index of the loudest hit — pocket anchor for swing / timing.
pub fn main_hit_index(plan: &OrnamentPlan) -> usize {
    let mut best = 0usize;
    let mut best_vel = 0u8;
    for i in 0..plan.len as usize {
        let v = plan.hits[i].vel_pct;
        if v >= best_vel {
            best_vel = v;
            best = i;
        }
    }
    best
}

/// Map plan offsets into strictly ascending dues within the parent step.
pub fn hit_due_ms(anchor_due_ms: u32, plan: &OrnamentPlan, sixteenth_ms: u32) -> [u32; MAX_HITS] {
    map_plan_dues(anchor_due_ms, plan, sixteenth_ms.max(8), 1)
}

/// Tick-domain variant for Bassment / Contura (parent step = `parent_ticks` clock ticks).
pub fn hit_due_ticks(anchor_due: u32, plan: &OrnamentPlan, parent_ticks: u32) -> [u32; MAX_HITS] {
    map_plan_dues(anchor_due, plan, parent_ticks, 1)
}

#[inline]
pub fn scale_vel(parent: u16, hit: OrnamentHit) -> u16 {
    (parent as u32 * u32::from(hit.vel_pct) / 100).min(4095) as u16
}

/// Contura: chance gate from scale feel (no genre table).
pub fn contura_ornament_gate(
    feel_ornament: u16,
    express: u16,
    phrase_step: u8,
    phrase_len: u8,
    div: u32,
) -> u16 {
    // Fine grids leave no room for clean sub-hit spacing.
    if div < 4 {
        return 0;
    }
    let phrase_t = if phrase_len == 0 {
        0
    } else {
        (u32::from(phrase_step) * 600) / u32::from(phrase_len)
    };
    let base = feel_ornament
        .saturating_add(express / 4)
        .saturating_add(phrase_t as u16);
    (base / 4).min(2800)
}

/// Contura ornament kind from feel roll (melodic, not genre cliché).
pub fn contura_pick_kind(roll: u16, rising: bool) -> Option<OrnamentKind> {
    let r = roll % 1000;
    if r < 280 {
        Some(OrnamentKind::Flam)
    } else if r < 520 {
        Some(OrnamentKind::Double)
    } else if r < 720 && rising {
        Some(OrnamentKind::Triplet)
    } else if r < 850 {
        Some(OrnamentKind::Double)
    } else {
        None
    }
}

/// Contura plan — same timing templates; pitch moves happen in the app.
pub fn contura_plan(roll: u16, rising: bool, gate: u16) -> OrnamentPlan {
    if roll > gate {
        return OrnamentPlan::single_main();
    }
    match contura_pick_kind(roll, rising) {
        Some(kind) => plan_for_kind(kind),
        None => OrnamentPlan::single_main(),
    }
}

/// True when quantized dues fit inside `parent_ticks` with strict ascent.
pub fn contura_plan_fits(plan: &OrnamentPlan, parent_ticks: u32) -> bool {
    if plan.len <= 1 {
        return true;
    }
    if parent_ticks < 4 {
        return false;
    }
    let dues = hit_due_ticks(0, plan, parent_ticks);
    let len = plan.len as usize;
    if dues[..len].contains(&u32::MAX) {
        return false;
    }
    !dues[..len].windows(2).any(|w| w[1] <= w[0])
}

/// Bassment pitch for a sub-hit (monophonic). `hit_idx` 0..plan.len.
/// Scale-neighbor resolution stays in Bassment — this helper is chromatic/repeat only.
pub fn bass_ornament_pitch(
    main_note: u8,
    hit: OrnamentHit,
    hit_idx: usize,
    plan: &OrnamentPlan,
    pocket: bool,
    roll: u8,
) -> (u8, bool) {
    let is_main = hit_idx == main_hit_index(plan);
    if is_main {
        return (main_note, false);
    }
    let ghost = hit.vel_pct < 50;
    if ghost && roll < 40 {
        return (main_note.saturating_sub(1), true);
    }
    if hit.vel_pct < 45 && roll < 55 {
        return (main_note, true); // dead note — gate shortened by caller
    }
    match roll % 5 {
        0 if !pocket => (main_note.saturating_add(12).min(127), false),
        1 => (main_note.saturating_sub(1), ghost),
        2 => (main_note.saturating_add(1).min(127), ghost), // caller may replace with scale neighbor
        3 => (main_note, false),                            // repeat
        _ => (main_note.saturating_sub(2).max(1), ghost),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [OrnamentKind; 4] = [
        OrnamentKind::Double,
        OrnamentKind::Flam,
        OrnamentKind::Triplet,
        OrnamentKind::Roll,
    ];

    fn assert_dues_valid(dues: &[u32; MAX_HITS], len: usize, parent: u32) {
        let cap = parent.saturating_sub(1).max(1);
        for i in 0..len {
            assert!(
                dues[i] < parent,
                "due[{i}]={} must be < parent {parent}",
                dues[i]
            );
            assert!(dues[i] <= cap);
            if i > 0 {
                assert!(
                    dues[i] > dues[i - 1],
                    "dues must ascend: {:?}",
                    &dues[..len]
                );
            }
        }
    }

    #[test]
    fn all_kinds_anchor_zero_ticks6() {
        for kind in KINDS {
            let plan = plan_for_kind(kind);
            let dues = hit_due_ticks(0, &plan, 6);
            assert_dues_valid(&dues, plan.len as usize, 6);
        }
    }

    #[test]
    fn all_kinds_anchor_mid_ticks6() {
        for kind in KINDS {
            let plan = plan_for_kind(kind);
            let dues = hit_due_ticks(3, &plan, 6);
            assert_dues_valid(&dues, plan.len as usize, 6);
        }
    }

    #[test]
    fn all_kinds_anchor_near_end_ticks6() {
        for kind in KINDS {
            let plan = plan_for_kind(kind);
            let dues = hit_due_ticks(5, &plan, 6);
            assert_dues_valid(&dues, plan.len as usize, 6);
        }
    }

    #[test]
    fn all_kinds_anchor_zero_ms8() {
        for kind in KINDS {
            let plan = plan_for_kind(kind);
            let dues = hit_due_ms(0, &plan, 8);
            assert_dues_valid(&dues, plan.len as usize, 8);
        }
    }

    #[test]
    fn flam_no_collapse_at_anchor_zero() {
        let plan = plan_for_kind(OrnamentKind::Flam);
        let dues = hit_due_ticks(0, &plan, 6);
        assert_eq!(dues[0], 0);
        assert!(dues[1] > dues[0]);
    }

    #[test]
    fn main_on_anchor_when_plan_fits() {
        let plan = plan_for_kind(OrnamentKind::Double);
        let dues = hit_due_ms(12, &plan, 24);
        let main_i = main_hit_index(&plan);
        assert_eq!(dues[main_i], 12);
    }

    #[test]
    fn roll_anchor_near_end_ticks6_no_collision() {
        let plan = plan_for_kind(OrnamentKind::Roll);
        for anchor in [4, 5] {
            let dues = hit_due_ticks(anchor, &plan, 6);
            assert_dues_valid(&dues, plan.len as usize, 6);
            assert!(dues[2] > dues[1], "anchor={anchor}: {:?}", &dues[..3]);
            assert!(dues[1] > dues[0], "anchor={anchor}: {:?}", &dues[..3]);
        }
    }

    #[test]
    fn quantize_dues_handles_cap_collision() {
        let perms = [800u16, 900, 1000, 0];
        let dues = quantize_dues(&perms, 3, 6, 1);
        assert_eq!(dues[0], 3);
        assert_eq!(dues[1], 4);
        assert_eq!(dues[2], 5);
    }

    #[test]
    fn low_density_groove_off() {
        let ctx = ArticContext::Groove {
            density: 1000,
            feel: 3000,
        };
        assert_eq!(artic_intensity(ctx), 0);
    }

    #[test]
    fn max_density_zero_feel_groove_off() {
        let ctx = ArticContext::Groove {
            density: 4095,
            feel: 0,
        };
        assert_eq!(artic_intensity(ctx), 0);
    }

    #[test]
    fn groove_needs_density_and_feel() {
        let low_feel = ArticContext::Groove {
            density: 4095,
            feel: 512,
        };
        let high_both = ArticContext::Groove {
            density: 4095,
            feel: 4095,
        };
        assert!(artic_intensity(high_both) > artic_intensity(low_feel));
        assert!(artic_intensity(high_both) > 0);
    }

    #[test]
    fn fill_zero_feel_off() {
        let ctx = ArticContext::Fill { step: 14, feel: 0 };
        assert_eq!(artic_intensity(ctx), 0);
    }

    #[test]
    fn fill_high_feel_on() {
        let ctx = ArticContext::Fill {
            step: 14,
            feel: 4095,
        };
        assert!(artic_intensity(ctx) > 0);
    }

    #[test]
    fn solo_zero_feel_off() {
        let ctx = ArticContext::Solo {
            tier: 2,
            phrase_bar: 2,
            feel: 0,
        };
        assert_eq!(artic_intensity(ctx), 0);
    }

    #[test]
    fn solo_high_feel_on() {
        let ctx = ArticContext::Solo {
            tier: 2,
            phrase_bar: 2,
            feel: 4095,
        };
        assert!(artic_intensity(ctx) > 0);
    }

    #[test]
    fn entry_rate_stays_sparse() {
        let w = KindWeights {
            double: 30,
            flam: 10,
            triplet: 32,
            roll: 24,
        };
        let rate = entry_rate_pct(GrooveVoice::Hats, w, 100, 100);
        assert!(rate <= 36);
        let kick = entry_rate_pct(GrooveVoice::Kick, w, 100, 100);
        assert!(kick <= 14);
    }

    #[test]
    fn kind_pick_uses_full_weights() {
        let w = KindWeights {
            double: 10,
            flam: 10,
            triplet: 10,
            roll: 10,
        };
        let mut seen = [false; 4];
        for roll in 0..100u8 {
            if let Some(k) = pick_kind_proportional(w, roll) {
                seen[k as usize] = true;
            }
        }
        assert!(seen.iter().all(|&s| s));
    }
}
