//! Shared genre labels + LED colors for Grooves and Chord Vamp (Shift+Fader scrub).

use libfp::Color;

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
/// Discrete chrome for Enum/param UI; scrub LEDs use [`super::led_fx::spectrum_color`].
#[allow(dead_code)]
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
