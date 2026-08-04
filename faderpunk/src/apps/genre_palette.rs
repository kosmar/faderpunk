//! Shared genre labels + LED colors for Grooves and Chord Vamp (Shift+Fader scrub).

use libfp::Color;
use smart_leds::RGB8;

pub const NUM_GENRES: usize = 8;

/// Oldest → newest. Indices match Shift+Fader buckets and Enum params.
pub const GENRE_NAMES: &[&str] = &[
    "Dub",
    "Disco",
    "Hip-Hop",
    "House",
    "Techno",
    "Trip-Hop",
    "UK Garage",
    "Dubstep",
];

/// Same order as [`GENRE_NAMES`] — keep identical across apps.
pub const GENRE_COLORS: [Color; NUM_GENRES] = [
    Color::Orange, // Dub
    Color::Yellow, // Disco
    Color::Red,    // Hip-Hop
    Color::Pink,   // House
    Color::Cyan,   // Techno
    Color::Violet, // Trip-Hop
    Color::Green,  // UK Garage
    Color::Blue,   // Dubstep
];

/// Fader position at the center of genre bucket `index` (seeds Alt latch target).
pub fn genre_fader_center(index: usize, picks: usize) -> u16 {
    let picks = picks.max(1);
    let i = index.min(picks - 1) as u32;
    let p = picks as u32;
    ((((i * 2) + 1) * 4095) / (p * 2)) as u16
}

fn slot_color(index: usize, extra: Color) -> Color {
    if index < NUM_GENRES {
        GENRE_COLORS[index]
    } else {
        extra
    }
}

fn lerp_u8(a: u8, b: u8, frac: u32) -> u8 {
    let frac = frac.min(255);
    ((a as u32 * (255 - frac) + b as u32 * frac) / 255) as u8
}

/// Soft RGB blend across genre slots along the fader (same buckets as
/// [`libfp::utils::value_to_index`], but continuous between neighbors).
///
/// `picks` is usually [`NUM_GENRES`], or `NUM_GENRES + 1` when Vamp adds Capture.
/// Slots at `index >= NUM_GENRES` use `extra` (e.g. Capture Rose).
pub fn genre_fader_color(fader: u16, picks: usize, extra: Color) -> Color {
    let picks = picks.max(1);
    let f = fader.min(4095) as u32;
    // Continuous position in [0, picks) with 8-bit fraction — matches
    // value_to_index(fader, picks) for the integer part.
    let pos256 = (f * picks as u32 * 256) / 4096;
    let i = (pos256 / 256).min(picks.saturating_sub(1) as u32) as usize;
    let frac = pos256 % 256;
    let j = (i + 1).min(picks - 1);
    if j == i || frac == 0 {
        return slot_color(i, extra);
    }
    let a: RGB8 = slot_color(i, extra).into();
    let b: RGB8 = slot_color(j, extra).into();
    Color::Custom(
        lerp_u8(a.r, b.r, frac),
        lerp_u8(a.g, b.g, frac),
        lerp_u8(a.b, b.b, frac),
    )
}
