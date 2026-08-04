//! Shared IR→UV fader LED helpers for WIP apps (no red→red wrap).

use libfp::{utils::split_unsigned_value, Brightness, Color};
use smart_leds::RGB8;

use crate::app::{Led, Leds};

/// HSV with explicit S/V (0..=255). Hue in degrees 0..360.
pub fn hsv_to_rgb(hue: u16, sat: u8, val: u8) -> (u8, u8, u8) {
    if sat == 0 {
        return (val, val, val);
    }
    let sector = (hue % 360) / 60;
    let f = (hue % 60) as u32;
    let p = (val as u32 * (255 - sat as u32) / 255) as u8;
    let q = (val as u32 * (255 - (sat as u32 * f) / 60) / 255) as u8;
    let t = (val as u32 * (255 - (sat as u32 * (60 - f)) / 60) / 255) as u8;
    match sector {
        0 => (val, t, p),
        1 => (q, val, p),
        2 => (p, val, t),
        3 => (p, q, val),
        4 => (t, p, val),
        _ => (val, p, q),
    }
}

/// Infrared → ultraviolet (0°..270°): red → yellow → green → cyan → blue → violet.
/// Full sat/value — fader level is shown via Top/Bottom brightness meters.
pub fn spectrum_color(fader: u16) -> Color {
    let hue = (fader.min(4095) as u32 * 270 / 4095) as u16;
    let (r, g, b) = hsv_to_rgb(hue, 255, 255);
    Color::Custom(r, g, b)
}

/// Paint Top/Bottom bipolar meter + Button brightness with a solid color.
pub fn paint_fader_meters<const N: usize>(
    leds: &Leds<N>,
    color: Color,
    fader: u16,
    button_bright: u8,
) {
    let led = split_unsigned_value(fader);
    leds.set(0, Led::Top, color, Brightness::Custom(led[0].max(12)));
    leds.set(0, Led::Bottom, color, Brightness::Custom(led[1].max(12)));
    leds.set(
        0,
        Led::Button,
        color,
        Brightness::Custom(button_bright.max(24)),
    );
}

/// Soft blend between two named palette colors along a fader (0..=4095).
#[allow(dead_code)]
pub fn blend_colors(a: Color, b: Color, fader: u16) -> Color {
    let frac = (fader.min(4095) as u32 * 255) / 4095;
    let aa: RGB8 = a.into();
    let bb: RGB8 = b.into();
    let lerp = |x: u8, y: u8| ((x as u32 * (255 - frac) + y as u32 * frac) / 255) as u8;
    Color::Custom(lerp(aa.r, bb.r), lerp(aa.g, bb.g), lerp(aa.b, bb.b))
}
