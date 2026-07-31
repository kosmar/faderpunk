use embassy_futures::{
    join::join4,
    select::{select, select3, Either},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use heapless::Vec;
use midly::{num::u7, MidiMessage};
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue,
    latch::LatchLayer,
    quantizer::Pitch,
    utils::{split_unsigned_value, value_to_index},
    AppIcon, Brightness, Color, Config, MidiChannel, MidiIn, MidiNote, MidiOut, Note, Param, Range,
    Value, VoltPerOct, APP_MAX_PARAMS,
};

use crate::app::{
    App, AppParams, AppStorage, Led, ManagedStorage, MidiOutput, ParamStore, Quantizer, SceneEvent,
};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 9;

const LED_BRIGHTNESS: Brightness = Brightness::Mid;
const NUM_CHORD_TYPES: usize = 7;
const MAX_VOICES: usize = 4;
const SPREAD_STEPS: usize = 4;

const IO_MIDI_MIDI: usize = 0;
const IO_MIDI_CV: usize = 1;
const IO_CV_MIDI: usize = 2;

/// CV→MIDI: quantized note must hold this many 1 ms frames to count as stable.
const CV_STABLE_MS: u16 = 12;
/// Sliding window for peak-to-peak "slew / unplug" detection.
const CV_HIST: usize = 32;
/// Peak-to-peak above this → transitioning / floating HF noise (not a held pitch).
const CV_NOISE_PP: u16 = 96;

/// CV→MIDI voice state: park the first stable float, only play after a pitch change
/// (or after a noisy slew settles while already live).
#[derive(Clone, Copy, PartialEq)]
enum CvVoice {
    Idle,
    /// Stable pitch seen, silent — kills unpatched mid-rail drones.
    Armed { note: u8 },
    /// Sounding; brief noise keeps us live so stepped CV still retriggers.
    Playing { note: u8 },
    /// Was playing, ADC moving — next stable note resumes Playing.
    Slewing,
}

/// Semitone offsets from root (index 0 is always the root).
const CHORD_TEMPLATES: &[&[i8]] = &[
    &[0],           // Unison
    &[0, 7],        // Power
    &[0, 3, 7],     // Minor
    &[0, 4, 7],     // Major
    &[0, 5, 7],     // Sus4
    &[0, 3, 7, 10], // Min7
    &[0, 4, 7, 10], // Dom7
];

pub static CONFIG: Config<PARAMS> = Config::new(
    "Harmonica",
    "Monophonic MIDI/CV harmonizer",
    Color::Orange,
    AppIcon::Note,
)
.add_param(Param::Enum {
    name: "I/O",
    variants: &["MIDI→MIDI", "MIDI→CV", "CV→MIDI"],
})
.add_param(Param::Range {
    name: "Range",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::Color {
    name: "Color",
    variants: &[
        Color::Blue,
        Color::Green,
        Color::Rose,
        Color::Orange,
        Color::Cyan,
        Color::Pink,
        Color::Violet,
        Color::Yellow,
    ],
})
.add_param(Param::MidiIn)
.add_param(Param::MidiChannel {
    name: "MIDI In CH",
})
.add_param(Param::MidiOut)
.add_param(Param::MidiChannel {
    name: "MIDI Out CH",
})
.add_param(Param::VoltPerOct)
.add_param(Param::bool {
    name: "Bypass quantizer",
});

pub struct Params {
    io_mode: usize,
    range: Range,
    color: Color,
    midi_in: MidiIn,
    midi_in_ch: MidiChannel,
    midi_out: MidiOut,
    midi_out_ch: MidiChannel,
    vpo: VoltPerOct,
    bypass: bool,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < PARAMS {
            return None;
        }
        Some(Self {
            io_mode: usize::from_value(values[0]),
            range: Range::from_value(values[1]),
            color: Color::from_value(values[2]),
            midi_in: MidiIn::from_value(values[3]),
            midi_in_ch: MidiChannel::from_value(values[4]),
            midi_out: MidiOut::from_value(values[5]),
            midi_out_ch: MidiChannel::from_value(values[6]),
            vpo: VoltPerOct::from_value(values[7]),
            bypass: bool::from_value(values[8]),
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.io_mode.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.color.into()).unwrap();
        vec.push(self.midi_in.into()).unwrap();
        vec.push(self.midi_in_ch.into()).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.midi_out_ch.into()).unwrap();
        vec.push(self.vpo.into()).unwrap();
        vec.push(self.bypass.into()).unwrap();
        vec
    }
}

#[derive(Serialize, Deserialize)]
pub struct Storage {
    chord_saved: u16,
    spread_saved: u16,
    vel_saved: u16,
    /// 0 = −1 oct, 1 = 0, 2 = +1
    octave_idx: u8,
    muted: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            chord_saved: 0,
            spread_saved: 0,
            vel_saved: 4095,
            octave_idx: 1,
            muted: false,
        }
    }
}

impl AppStorage for Storage {}

fn midi_u8(note: MidiNote) -> u8 {
    u7::from(note).as_int()
}

fn midi_to_pitch(midi: u8) -> Pitch {
    Pitch {
        octave: (midi as i32 / 12 - 1) as i8,
        note: Note::from(midi % 12),
        raw: None,
    }
}

fn octave_from_idx(idx: u8) -> i8 {
    match idx {
        0 => -1,
        2 => 1,
        _ => 0,
    }
}

fn unique_push(out: &mut Vec<u8, MAX_VOICES>, note: u8) {
    if out.contains(&note) {
        return;
    }
    let _ = out.push(note);
}

struct VoiceParams {
    root: u8,
    chord_type: usize,
    spread: u16,
    octave_idx: u8,
    muted: bool,
    range: Range,
    vpo: VoltPerOct,
    bypass: bool,
}

/// Hybrid chord: chromatic template, then snap non-root tones to the global scale.
async fn build_voices(quantizer: &Quantizer, p: VoiceParams) -> Vec<u8, MAX_VOICES> {
    let mut out: Vec<u8, MAX_VOICES> = Vec::new();
    unique_push(&mut out, p.root);
    if p.muted {
        return out;
    }

    let template = CHORD_TEMPLATES[p.chord_type.min(NUM_CHORD_TYPES - 1)];
    let spread_steps = value_to_index(p.spread, SPREAD_STEPS) as i16;
    let harm_oct = octave_from_idx(p.octave_idx) as i16 * 12;

    for (vi, &semis) in template.iter().enumerate() {
        if semis == 0 {
            continue;
        }
        let mut note = (p.root as i16 + semis as i16).clamp(0, 127) as u8;
        if !p.bypass {
            let counts = midi_to_pitch(note).as_counts(p.range, p.vpo);
            note = midi_u8(quantizer.get_quantized_note(counts).await.as_midi());
        }
        note = (note as i16 + harm_oct + 12 * spread_steps * vi as i16).clamp(0, 127) as u8;
        unique_push(&mut out, note);
    }
    out
}

async fn revoice(
    midi: &MidiOutput,
    old: &Vec<u8, MAX_VOICES>,
    new: &Vec<u8, MAX_VOICES>,
    root: Option<u8>,
    root_vel: u16,
    harm_vel: u16,
) {
    for &n in old.iter() {
        if !new.contains(&n) {
            midi.send_note_off(MidiNote::from(n)).await;
        }
    }
    for &n in new.iter() {
        if !old.contains(&n) {
            let vel = if Some(n) == root { root_vel } else { harm_vel };
            midi.send_note_on(MidiNote::from(n), vel).await;
        }
    }
}

async fn all_notes_off(midi: &MidiOutput, sounding: &mut Vec<u8, MAX_VOICES>) {
    for &n in sounding.iter() {
        midi.send_note_off(MidiNote::from(n)).await;
    }
    sounding.clear();
}

#[embassy_executor::task(pool_size = 16 / CHANNELS)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(
        app.app_id,
        app.layout_id,
        Params {
            io_mode: IO_MIDI_MIDI,
            range: Range::_0_10V,
            color: Color::Orange,
            midi_in: MidiIn::default(),
            midi_in_ch: MidiChannel::default(),
            midi_out: MidiOut::default(),
            midi_out_ch: MidiChannel::from(2),
            vpo: VoltPerOct::Standard,
            bypass: false,
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
    let (io_mode, range, led_color, midi_in_cfg, midi_in_ch, midi_out_cfg, midi_out_ch, vpo, bypass) =
        params.query(|p| {
            (
                p.io_mode,
                p.range,
                p.color,
                p.midi_in,
                p.midi_in_ch,
                p.midi_out,
                p.midi_out_ch,
                p.vpo,
                p.bypass,
            )
        });

    let fader = app.use_faders();
    let buttons = app.use_buttons();
    let leds = app.use_leds();
    let mut midi_in = app.use_midi_input(midi_in_cfg, midi_in_ch);
    let midi = app.use_midi_output(midi_out_cfg, midi_out_ch, false);
    let quantizer = app.use_quantizer(range, vpo, bypass);

    let out_jack = if io_mode == IO_MIDI_CV {
        Some(app.make_out_jack(0, range).await)
    } else {
        None
    };
    let in_jack = if io_mode == IO_CV_MIDI {
        Some(app.make_in_jack(0, range).await)
    } else {
        None
    };

    let (chord_saved, spread_saved, vel_saved, octave_idx, muted) = storage.query(|s| {
        (
            s.chord_saved,
            s.spread_saved,
            s.vel_saved,
            s.octave_idx,
            s.muted,
        )
    });

    let muted_glob = app.make_global(muted);
    let octave_glob = app.make_global(octave_idx);
    let chord_glob = app.make_global(chord_saved);
    let spread_glob = app.make_global(spread_saved);
    let vel_glob = app.make_global(vel_saved);
    let latch_glob = app.make_global(LatchLayer::Main);
    let long_press_fired = app.make_global(false);
    let panic_flag = app.make_global(false);
    let revoice_flag = app.make_global(true);
    let root_glob = app.make_global(None::<u8>);
    let root_vel_glob = app.make_global(4095u16);

    if muted {
        leds.unset(0, Led::Button);
    } else {
        leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
    }

    let engine = async {
        let mut latch = app.make_latch(fader.get_value());
        let mut sounding: Vec<u8, MAX_VOICES> = Vec::new();
        let mut cv_voice = CvVoice::Idle;
        let mut cv_stable_note: Option<u8> = None;
        let mut cv_stable_count = 0u16;
        let mut cv_hist = [0u16; CV_HIST];
        let mut cv_hist_i = 0usize;
        let mut cv_hist_fill = 0usize;
        let mut cv_slew_ms = 0u16;

        loop {
            let midi_msg = if io_mode == IO_CV_MIDI {
                app.delay_millis(1).await;
                None
            } else {
                match select(midi_in.wait_for_message(), app.delay_millis(1)).await {
                    Either::First(msg) => Some(msg),
                    Either::Second(_) => None,
                }
            };

            // ── Latch layers ──────────────────────────────────────────────
            let latch_layer = if buttons.is_shift_pressed() && !buttons.is_button_pressed(0) {
                LatchLayer::Alt
            } else if !buttons.is_shift_pressed() && buttons.is_button_pressed(0) {
                LatchLayer::Third
            } else {
                LatchLayer::Main
            };
            latch_glob.set(latch_layer);

            let chord_val = chord_glob.get();
            let spread_val = spread_glob.get();
            let vel_val = vel_glob.get();
            let target = match latch_layer {
                LatchLayer::Main => chord_val,
                LatchLayer::Alt => spread_val,
                LatchLayer::Third => vel_val,
            };

            if let Some(new_value) = latch.update(fader.get_value(), latch_layer, target) {
                match latch_layer {
                    LatchLayer::Main => {
                        chord_glob.set(new_value);
                        storage.modify(|s| s.chord_saved = new_value);
                        revoice_flag.set(true);
                    }
                    LatchLayer::Alt => {
                        spread_glob.set(new_value);
                        storage.modify(|s| s.spread_saved = new_value);
                        revoice_flag.set(true);
                    }
                    LatchLayer::Third => {
                        vel_glob.set(new_value);
                        storage.modify(|s| s.vel_saved = new_value);
                    }
                }
            }

            // ── Panic ─────────────────────────────────────────────────────
            if panic_flag.get() {
                panic_flag.set(false);
                all_notes_off(&midi, &mut sounding).await;
                root_glob.set(None);
                cv_voice = CvVoice::Idle;
                cv_stable_note = None;
                cv_stable_count = 0;
                cv_hist_fill = 0;
                cv_hist_i = 0;
                cv_slew_ms = 0;
                if let Some(ref jack) = out_jack {
                    jack.set_value(0);
                }
            }

            // ── MIDI input (MIDI→MIDI / MIDI→CV) ──────────────────────────
            if let Some(msg) = midi_msg {
                match msg {
                    MidiMessage::NoteOn { key, vel } if vel > 0 => {
                        let key_n = key.as_int();
                        let vel12 = ((vel.as_int() as u32 * 4095) / 127) as u16;
                        root_glob.set(Some(key_n));
                        root_vel_glob.set(vel12);
                        revoice_flag.set(true);
                    }
                    MidiMessage::NoteOn { key, vel } if vel == 0 => {
                        let key_n = key.as_int();
                        if root_glob.get() == Some(key_n) {
                            all_notes_off(&midi, &mut sounding).await;
                            root_glob.set(None);
                            if let Some(ref jack) = out_jack {
                                jack.set_value(0);
                            }
                        }
                    }
                    MidiMessage::NoteOff { key, .. } => {
                        let key_n = key.as_int();
                        if root_glob.get() == Some(key_n) {
                            all_notes_off(&midi, &mut sounding).await;
                            root_glob.set(None);
                            if let Some(ref jack) = out_jack {
                                jack.set_value(0);
                            }
                        }
                    }
                    _ => {}
                }
            }

            // ── CV→MIDI: park float, play on pitch change / post-slew ─────
            if let Some(ref jack) = in_jack {
                let raw = jack.get_value();
                cv_hist[cv_hist_i] = raw;
                cv_hist_i = (cv_hist_i + 1) % CV_HIST;
                if cv_hist_fill < CV_HIST {
                    cv_hist_fill += 1;
                }

                let noisy = if cv_hist_fill >= CV_HIST {
                    let (mn, mx) = cv_hist.iter().fold((u16::MAX, 0u16), |(a, b), &v| {
                        (a.min(v), b.max(v))
                    });
                    mx.saturating_sub(mn) > CV_NOISE_PP
                } else {
                    false
                };

                let pitched = quantizer.get_quantized_note(raw).await;
                let note = midi_u8(pitched.as_midi());
                if cv_stable_note == Some(note) {
                    cv_stable_count = cv_stable_count.saturating_add(1);
                } else {
                    cv_stable_note = Some(note);
                    cv_stable_count = 1;
                }
                let locked = cv_stable_count >= CV_STABLE_MS;

                let mut start_note: Option<u8> = None;

                match cv_voice {
                    CvVoice::Idle => {
                        cv_slew_ms = 0;
                        if !noisy && locked {
                            // Park — do not sound (unpatched mid-rail stays here).
                            cv_voice = CvVoice::Armed { note };
                        }
                    }
                    CvVoice::Armed { note: armed } => {
                        cv_slew_ms = 0;
                        if noisy {
                            cv_voice = CvVoice::Idle;
                        } else if locked && note != armed {
                            // Real pitch change after park → start sounding.
                            cv_voice = CvVoice::Playing { note };
                            start_note = Some(note);
                        }
                    }
                    CvVoice::Playing { note: playing } => {
                        cv_slew_ms = 0;
                        if noisy {
                            cv_voice = CvVoice::Slewing;
                            cv_slew_ms = 1;
                        } else if locked && note != playing {
                            cv_voice = CvVoice::Playing { note };
                            start_note = Some(note);
                        }
                    }
                    CvVoice::Slewing => {
                        if !noisy && locked {
                            cv_voice = CvVoice::Playing { note };
                            cv_slew_ms = 0;
                            start_note = Some(note);
                        } else {
                            cv_slew_ms = cv_slew_ms.saturating_add(1);
                            // Unplug / float: don't hold the last chord forever.
                            if cv_slew_ms > 80 {
                                cv_voice = CvVoice::Idle;
                                cv_slew_ms = 0;
                            }
                        }
                    }
                }

                let keep_ringout = matches!(cv_voice, CvVoice::Slewing) && cv_slew_ms <= 80;
                if !keep_ringout
                    && !matches!(cv_voice, CvVoice::Playing { .. })
                    && start_note.is_none()
                    && (root_glob.get().is_some() || !sounding.is_empty())
                {
                    all_notes_off(&midi, &mut sounding).await;
                    root_glob.set(None);
                }
                if let Some(n) = start_note {
                    root_glob.set(Some(n));
                    root_vel_glob.set(4095);
                    revoice_flag.set(true);
                }
            }

            // ── Live revoice ──────────────────────────────────────────────
            if revoice_flag.get() {
                revoice_flag.set(false);
                if let Some(root) = root_glob.get() {
                    let chord_type = value_to_index(chord_glob.get(), NUM_CHORD_TYPES);
                    let new_voices = build_voices(
                        &quantizer,
                        VoiceParams {
                            root,
                            chord_type,
                            spread: spread_glob.get(),
                            octave_idx: octave_glob.get(),
                            muted: muted_glob.get(),
                            range,
                            vpo,
                            bypass,
                        },
                    )
                    .await;

                    if io_mode != IO_MIDI_CV {
                        revoice(
                            &midi,
                            &sounding,
                            &new_voices,
                            Some(root),
                            root_vel_glob.get(),
                            vel_glob.get(),
                        )
                        .await;
                    }
                    sounding = new_voices;
                } else if !sounding.is_empty() {
                    all_notes_off(&midi, &mut sounding).await;
                }
            }

            // ── MIDI→CV jack ──────────────────────────────────────────────
            if let Some(ref jack) = out_jack {
                if let Some(&top) = sounding.iter().max() {
                    jack.set_value(midi_to_pitch(top).as_counts(range, vpo));
                } else if let Some(root) = root_glob.get() {
                    jack.set_value(midi_to_pitch(root).as_counts(range, vpo));
                }
            }

            // ── LEDs ──────────────────────────────────────────────────────
            match latch_layer {
                LatchLayer::Main => {
                    let chord_type = value_to_index(chord_glob.get(), NUM_CHORD_TYPES);
                    let level = ((chord_type as u32 * 255) / (NUM_CHORD_TYPES as u32 - 1)) as u8;
                    leds.set(0, Led::Top, led_color, Brightness::Custom(level));
                    let oct = octave_glob.get();
                    let oct_bri = match oct {
                        0 => 40,
                        2 => 255,
                        _ => 120,
                    };
                    leds.set(0, Led::Bottom, led_color, Brightness::Custom(oct_bri));
                }
                LatchLayer::Alt => {
                    let bri = (spread_glob.get() / 16) as u8;
                    leds.set(0, Led::Top, Color::Red, Brightness::Custom(bri));
                    leds.unset(0, Led::Bottom);
                }
                LatchLayer::Third => {
                    let bri = (vel_glob.get() / 16) as u8;
                    leds.set(0, Led::Top, Color::Red, Brightness::Custom(bri));
                    leds.set(0, Led::Bottom, Color::Red, Brightness::Custom(bri));
                }
            }

            if let Some(&top) = sounding.iter().max() {
                let led = split_unsigned_value((top as u16).saturating_mul(32));
                if latch_layer == LatchLayer::Main {
                    leds.set(0, Led::Top, led_color, Brightness::Custom(led[0].saturating_mul(2)));
                }
            }
        }
    };

    let button_handler = async {
        // Mute = short tap without fader move (Third = Button+Fader velocity).
        // Do not gate mute on latch Third updates — entering Third alone often
        // commits a pickup/jump and would swallow every mute tap.
        const FADER_MOVE_DEADBAND: u16 = 48;
        loop {
            let shift = buttons.wait_for_down(0).await;
            if shift {
                // Alt+press: cycle harmony octave −1 → 0 → +1
                let next = match octave_glob.get() {
                    0 => 1,
                    1 => 2,
                    _ => 0,
                };
                octave_glob.set(next);
                storage.modify_and_save(|s| s.octave_idx = next);
                revoice_flag.set(true);
                buttons.wait_for_up(0).await;
            } else {
                long_press_fired.set(false);
                let fader_at_down = fader.get_value();
                buttons.wait_for_up(0).await;
                let moved = fader
                    .get_value()
                    .abs_diff(fader_at_down)
                    >= FADER_MOVE_DEADBAND;
                if !moved {
                    if long_press_fired.get() {
                        panic_flag.set(true);
                    } else {
                        let muted = !muted_glob.get();
                        muted_glob.set(muted);
                        storage.modify_and_save(|s| s.muted = muted);
                        revoice_flag.set(true);
                        if muted {
                            leds.unset(0, Led::Button);
                        } else {
                            leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
                        }
                    }
                }
            }
        }
    };

    let long_press_handler = async {
        loop {
            // Mark only — panic runs on release if the fader wasn't moved
            // (so Button+Fader velocity edits never wipe notes mid-hold).
            let _ = buttons.wait_for_any_long_press().await;
            long_press_fired.set(true);
        }
    };

    let scene_handler = async {
        loop {
            match app.wait_for_scene_event().await {
                SceneEvent::LoadScene(scene) => {
                    storage.load_from_scene(scene).await;
                    let (chord, spread, vel, oct, muted) = storage.query(|s| {
                        (
                            s.chord_saved,
                            s.spread_saved,
                            s.vel_saved,
                            s.octave_idx,
                            s.muted,
                        )
                    });
                    chord_glob.set(chord);
                    spread_glob.set(spread);
                    vel_glob.set(vel);
                    octave_glob.set(oct);
                    muted_glob.set(muted);
                    revoice_flag.set(true);
                    if muted {
                        leds.unset(0, Led::Button);
                    } else {
                        leds.set(0, Led::Button, led_color, LED_BRIGHTNESS);
                    }
                }
                SceneEvent::SaveScene(scene) => storage.save_to_scene(scene).await,
            }
        }
    };

    join4(engine, button_handler, long_press_handler, scene_handler).await;
}
