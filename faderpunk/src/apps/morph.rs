//! Morph wave engine shared by Super LFO and Manifold.
//!
//! A single fader sweeps a continuum of waveform nodes (soft waves → stepped /
//! chaos) with phase-shaping (symmetry, warp, skew) on top. `morph_color`
//! renders the same node hue both apps use, so a morph fader always looks the
//! same wherever it appears.
//!
//! Lives in the firmware crate rather than `libfp` because the chaos nodes draw
//! from the hardware RNG (`Die`).

use libfp::{Color, Waveform};

use crate::app::Die;

/// Morph continuum: soft waves → stepped/chaos.
/// Indices: 0 Sine, 1 Tri, 2 Saw, 3 Square, 4 Walk, 5 S&H, 6 Noise
pub const MORPH_NODES: usize = 7;
/// Hue (degrees 0–359) for each morph node — full saturation at the node,
/// desaturates toward the next node so the waveform type is always readable.
const NODE_HUES: [u16; MORPH_NODES] = [0, 45, 90, 135, 180, 225, 270];
/// How hard Symmetry leans away from 50% for a given fader offset (< 1 = more extreme).
const SYMMETRY_LEAN_CURVE: f32 = 0.45;

pub fn symmetry_phase(phase: usize, symmetry: u16) -> usize {
    // Piecewise phase remap: center (2048) = balanced halves.
    // Lean curve pulls toward extremes faster so shape changes read clearly live.
    let t = (phase % 4096) as f32 / 4096.0;
    let centered = (symmetry as f32 / 4095.0 - 0.5) * 2.0; // -1..1
    let lean = libm::copysignf(libm::powf(centered.abs(), SYMMETRY_LEAN_CURVE), centered);
    let pw = (0.5 + lean * 0.49).clamp(0.01, 0.99);
    let out = if t < pw {
        t / pw * 0.5
    } else {
        0.5 + (t - pw) / (1.0 - pw) * 0.5
    };
    (out.clamp(0.0, 1.0) * 4095.0) as usize
}

pub fn warp_phase(phase: usize, warp: u16) -> usize {
    if warp == 0 {
        return phase % 4096;
    }
    let t = (phase % 4096) as f32 / 4096.0;
    let amount = warp as f32 / 4095.0;
    // Smoothstep blend toward ease-in/out time feel
    let eased = t * t * (3.0 - 2.0 * t);
    let out = t * (1.0 - amount) + eased * amount;
    (out * 4095.0) as usize
}

pub fn skew_phase(phase: usize, skew: u16) -> usize {
    // Center (2048) = linear; low/high lean soft asymmetry (pow curve)
    let t = (phase % 4096) as f32 / 4096.0;
    let s = (skew as f32 / 4095.0 - 0.5) * 2.0; // -1..1
    let warped = if s >= 0.0 {
        libm::powf(t, 1.0 + s)
    } else {
        1.0 - libm::powf(1.0 - t, 1.0 - s)
    };
    (warped.clamp(0.0, 1.0) * 4095.0) as usize
}

#[derive(Clone, Copy)]
pub struct MorphChaos {
    walk_a: i32,
    walk_b: i32,
    sh_a: u16,
    sh_b: u16,
    sh_bucket_a: u16,
    sh_bucket_b: u16,
}

impl MorphChaos {
    pub fn new() -> Self {
        Self {
            walk_a: 2048,
            walk_b: 2048,
            sh_a: 2048,
            sh_b: 2048,
            sh_bucket_a: 0xffff,
            sh_bucket_b: 0xffff,
        }
    }

    pub fn tick_walks(&mut self, die: &Die) {
        // Gentle drift (~±3 at 1 kHz audio tick).
        let step_a = (die.roll() as i32 % 7) - 3;
        let step_b = (die.roll() as i32 % 7) - 3;
        self.walk_a = (self.walk_a + step_a).clamp(0, 4095);
        self.walk_b = (self.walk_b + step_b).clamp(0, 4095);
    }
}

fn classic_wave(node: usize, phase: usize) -> Option<u16> {
    let w = match node {
        0 => Waveform::Sine,
        1 => Waveform::Triangle,
        2 => Waveform::Saw,
        3 => Waveform::Square,
        _ => return None,
    };
    Some(w.at(phase))
}

fn chaos_sample(node: usize, phase: usize, osc: usize, chaos: &mut MorphChaos, die: &Die) -> u16 {
    match node {
        4 => {
            if osc == 0 {
                chaos.walk_a as u16
            } else {
                chaos.walk_b as u16
            }
        }
        5 => {
            // S&H — new level every 1/16 of the cycle (phase bucket).
            let bucket = (phase / 256) as u16;
            let (sh, last) = if osc == 0 {
                (&mut chaos.sh_a, &mut chaos.sh_bucket_a)
            } else {
                (&mut chaos.sh_b, &mut chaos.sh_bucket_b)
            };
            if bucket != *last {
                *last = bucket;
                *sh = die.roll();
            }
            *sh
        }
        _ => die.roll(),
    }
}

fn node_sample(node: usize, phase: usize, osc: usize, chaos: &mut MorphChaos, die: &Die) -> u16 {
    classic_wave(node, phase).unwrap_or_else(|| chaos_sample(node, phase, osc, chaos, die))
}

/// `form` = (skew, warp, symmetry). `osc` selects which chaos state a stepped
/// node draws from, so two oscillators stay decorrelated.
pub fn morph_sample(
    phase: usize,
    morph: u16,
    form: (u16, u16, u16),
    osc: usize,
    chaos: &mut MorphChaos,
    die: &Die,
) -> u16 {
    let (skew, warp, symmetry) = form;
    let p = skew_phase(symmetry_phase(warp_phase(phase, warp), symmetry), skew);
    let segments = MORPH_NODES - 1;
    let seg_size = 4096 / segments;
    let raw_seg = (morph as usize) / seg_size;
    // Past the last node (morph 4092–4095 due to 4096/6 remainder): pure Noise.
    if raw_seg >= segments {
        return node_sample(MORPH_NODES - 1, p, osc, chaos, die);
    }
    let frac = (morph as usize) % seg_size;
    let a = node_sample(raw_seg, p, osc, chaos, die) as i32;
    let b = node_sample(raw_seg + 1, p, osc, chaos, die) as i32;
    (a + (b - a) * frac as i32 / seg_size as i32).clamp(0, 4095) as u16
}

/// Node-snap morph color: full saturation at each waveform anchor, linearly
/// desaturates toward the next node. The new hue snaps in at full saturation
/// the moment the fader crosses a node boundary. The last node (Noise) has no
/// successor, so its segment stays at full blue saturation throughout.
pub fn morph_color(morph: u16) -> Color {
    let segments = MORPH_NODES - 1; // 6
    let seg_size = 4096 / segments; // 682
    let m = morph.min(4095) as usize;
    let seg = (m / seg_size).min(segments - 1);
    let frac = m % seg_size;
    // Last segment (S&H → Noise): no next node, so Noise blue throughout.
    if seg == segments - 1 {
        let (r, g, b) = hsv_to_rgb(NODE_HUES[MORPH_NODES - 1]);
        return Color::Custom(r, g, b);
    }
    let sat = 255u8.saturating_sub((frac * 255 / seg_size) as u8);
    let (r, g, b) = hsv_to_rgb_sat(NODE_HUES[seg], sat);
    Color::Custom(r, g, b)
}

/// Integer HSV→RGB with V=max and variable saturation (0 = white, 255 = full hue).
fn hsv_to_rgb_sat(hue: u16, sat: u8) -> (u8, u8, u8) {
    let (fr, fg, fb) = hsv_to_rgb(hue);
    let s = sat as u32;
    let w = 255 - s;
    (
        ((fr as u32 * s + 255 * w) / 255) as u8,
        ((fg as u32 * s + 255 * w) / 255) as u8,
        ((fb as u32 * s + 255 * w) / 255) as u8,
    )
}

/// Integer HSV→RGB with S=V=max. Hue in degrees (0..360).
fn hsv_to_rgb(hue: u16) -> (u8, u8, u8) {
    let sector = hue / 60; // 0..=5
                           // Rising/falling ramp within the sector, scaled to 0..=255.
    let ramp = ((hue % 60) as u32 * 255 / 59) as u8;
    match sector {
        0 => (255, ramp, 0),
        1 => (255 - ramp, 255, 0),
        2 => (0, 255, ramp),
        3 => (0, 255 - ramp, 255),
        4 => (ramp, 0, 255),
        _ => (255, 0, 255 - ramp),
    }
}
