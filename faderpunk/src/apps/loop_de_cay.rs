//! Loop de Cay — clocked additive overdub loop with level decay.
//!
//! Gestures: Press = gate/play (mode-dependent), Long (no fader move) = mute
//! (decay pauses), Shift+Short = arm, Shift+Long = erase. Mode via Config
//! (Shift+Long cannot also cycle — same gesture as erase).

use embassy_futures::{
    join::join5,
    select::{select, select3, Either},
};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};
use embassy_time::Instant;
use heapless::Vec;
use midly::{num::u7, MidiMessage};
use serde::{Deserialize, Serialize};

use libfp::{
    ext::FromValue,
    latch::LatchLayer,
    AppIcon, Brightness, ClockDivision, Color, Config, MidiChannel, MidiIn, MidiNote, MidiOut,
    Param, Range, Value, VoltPerOct, APP_MAX_PARAMS,
};

use crate::app::{
    App, AppMidiEvent, AppParams, AppStorage, ClockEvent, Global, InJack, Led, ManagedStorage,
    MidiOutput, OutJack, ParamStore, Quantizer, SceneEvent,
};

pub const CHANNELS: usize = 1;
pub const PARAMS: usize = 9;

const TICKS_PER_16TH: u64 = 6;
const TICKS_PER_BAR: u16 = 96;
const MAX_BARS: u8 = 8;
const MAX_BUFFER_TICKS: u16 = TICKS_PER_BAR * MAX_BARS as u16;
const MAX_EVENTS: usize = 32;
const POLY: usize = 4;
const KILL_FLOOR: u16 = 32;
const FADER_MOVE_THRESH: u16 = 48;
const GATE_THRESH: u16 = 406;
const DEFAULT_GATE_TICKS: u16 = 12;
const ARM_BLINK_MS: u64 = 400;

pub static CONFIG: Config<PARAMS> = Config::new(
    "Loop de Cay",
    "Additive overdub loop with level decay",
    Color::Violet,
    AppIcon::Sequence,
)
.add_param(Param::Enum {
    name: "Mode",
    variants: &[
        "Pitch→MIDI",
        "Gate→MIDI",
        "MIDI→MIDI",
        "MIDI→CV",
        "Fader→Both",
    ],
})
.add_param(Param::MidiChannel {
    name: "MIDI Channel",
})
.add_param(Param::MidiIn)
.add_param(Param::MidiOut)
.add_param(Param::MidiNote {
    name: "Base Note",
})
.add_param(Param::i32 {
    name: "Span",
    min: 1,
    max: 120,
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
.add_param(Param::Range {
    name: "Range",
    variants: &[Range::_0_10V, Range::_Neg5_5V],
})
.add_param(Param::VoltPerOct);

pub struct Params {
    mode: usize,
    midi_channel: MidiChannel,
    midi_in: MidiIn,
    midi_out: MidiOut,
    midi_note: MidiNote,
    span: i32,
    color: Color,
    range: Range,
    vpo: VoltPerOct,
}

impl AppParams for Params {
    fn from_values(values: &[Value]) -> Option<Self> {
        if values.len() < PARAMS {
            return None;
        }
        Some(Self {
            mode: usize::from_value(values[0]).min(4),
            midi_channel: MidiChannel::from_value(values[1]),
            midi_in: MidiIn::from_value(values[2]),
            midi_out: MidiOut::from_value(values[3]),
            midi_note: MidiNote::from_value(values[4]),
            span: i32::from_value(values[5]),
            color: Color::from_value(values[6]),
            range: Range::from_value(values[7]),
            vpo: VoltPerOct::from_value(values[8]),
        })
    }

    fn to_values(&self) -> Vec<Value, APP_MAX_PARAMS> {
        let mut vec = Vec::new();
        vec.push(self.mode.into()).unwrap();
        vec.push(self.midi_channel.into()).unwrap();
        vec.push(self.midi_in.into()).unwrap();
        vec.push(self.midi_out.into()).unwrap();
        vec.push(self.midi_note.into()).unwrap();
        vec.push(self.span.into()).unwrap();
        vec.push(self.color.into()).unwrap();
        vec.push(self.range.into()).unwrap();
        vec.push(self.vpo.into()).unwrap();
        vec
    }
}

#[derive(Serialize, Deserialize)]
pub struct Storage {
    main_saved: u16,
    decay_saved: u16,
    bars_saved: u16,
    muted: bool,
    armed: bool,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            main_saved: 2048,
            decay_saved: 0,
            bars_saved: 0,
            muted: false,
            armed: false,
        }
    }
}
impl AppStorage for Storage {}

#[derive(Clone, Copy, Default)]
struct NoteEvent {
    start: u16,
    dur: u16,
    note: u8,
    vel: u16,
    gen: u32,
    used: bool,
}

#[derive(Clone, Copy)]
struct LoopBuf {
    events: [NoteEvent; MAX_EVENTS],
    next_gen: u32,
    extent: u16,
    origin: u64,
}

impl Default for LoopBuf {
    fn default() -> Self {
        Self {
            events: [NoteEvent::default(); MAX_EVENTS],
            next_gen: 1,
            extent: 0,
            origin: u64::MAX,
        }
    }
}

impl LoopBuf {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn window_ticks(bars: u8) -> u16 {
        (bars.max(1) as u16)
            .saturating_mul(TICKS_PER_BAR)
            .min(MAX_BUFFER_TICKS)
    }

    fn play_pos(&self, tick: u64, bars: u8) -> Option<u16> {
        if self.origin == u64::MAX || self.extent == 0 {
            return None;
        }
        let wt = Self::window_ticks(bars) as u64;
        if wt == 0 {
            return None;
        }
        Some(((tick.saturating_sub(self.origin)) % wt) as u16)
    }

    fn ensure_origin(&mut self, tick: u64) {
        if self.origin == u64::MAX {
            self.origin = tick - (tick % TICKS_PER_16TH);
        }
    }

    fn add_or_refresh(&mut self, start: u16, dur: u16, note: u8, vel: u16, window: u16) {
        let start = start % window.max(1);
        let start_q = start / TICKS_PER_16TH as u16;

        for e in self.events.iter_mut() {
            if e.used && e.note == note && e.start / TICKS_PER_16TH as u16 == start_q {
                e.vel = vel;
                e.dur = dur.max(1);
                e.gen = self.next_gen;
                self.next_gen = self.next_gen.wrapping_add(1);
                let end = start.saturating_add(if dur == u16::MAX { 1 } else { dur });
                self.extent = self.extent.max(end.min(MAX_BUFFER_TICKS)).max(window.min(MAX_BUFFER_TICKS));
                return;
            }
        }

        let slot_i = if let Some(i) = self.events.iter().position(|e| !e.used) {
            i
        } else {
            let mut oldest_i = 0;
            let mut oldest_gen = u32::MAX;
            for (i, e) in self.events.iter().enumerate() {
                if e.gen < oldest_gen {
                    oldest_gen = e.gen;
                    oldest_i = i;
                }
            }
            oldest_i
        };

        let slot = &mut self.events[slot_i];
        slot.used = true;
        slot.start = start;
        slot.dur = dur.max(1);
        slot.note = note;
        slot.vel = vel;
        slot.gen = self.next_gen;
        self.next_gen = self.next_gen.wrapping_add(1);

        let end = start.saturating_add(if dur == u16::MAX { 1 } else { dur });
        self.extent = self
            .extent
            .max(end.min(MAX_BUFFER_TICKS))
            .max(window.min(MAX_BUFFER_TICKS));
    }

    fn finalize_open(&mut self, note: u8, end_pos: u16, window: u16) {
        let end_pos = end_pos % window.max(1);
        for e in self.events.iter_mut() {
            if e.used && e.note == note && e.dur == u16::MAX {
                let start = e.start;
                let dur = if end_pos >= start {
                    (end_pos - start).max(1)
                } else {
                    (window - start + end_pos).max(1)
                };
                e.dur = dur.min(window);
                return;
            }
        }
    }

    fn decay_all(&mut self, decay_fader: u16) {
        if decay_fader == 0 {
            return;
        }
        let retain = 4095u32
            .saturating_sub((decay_fader as u32 * 3900) / 4095)
            .max(200);
        for e in self.events.iter_mut() {
            if !e.used {
                continue;
            }
            let nv = ((e.vel as u32 * retain) / 4095) as u16;
            if nv <= KILL_FLOOR {
                e.used = false;
                e.vel = 0;
            } else {
                e.vel = nv;
            }
        }
        if self.events.iter().all(|e| !e.used) {
            self.extent = 0;
            self.origin = u64::MAX;
        }
    }

    fn set_window_bars(&mut self, old_bars: u8, new_bars: u8) {
        let old_w = Self::window_ticks(old_bars);
        let new_w = Self::window_ticks(new_bars);
        if new_w <= old_w {
            return;
        }
        // Virgin extend: tile current audible window into new space.
        if self.extent > 0 && self.extent <= old_w {
            let tile_end = old_w.min(self.extent);
            if tile_end == 0 {
                return;
            }
            let mut extras: Vec<NoteEvent, MAX_EVENTS> = Vec::new();
            for e in self.events.iter() {
                if !e.used || e.start >= tile_end || e.dur == u16::MAX {
                    continue;
                }
                let mut pos = old_w;
                while pos < new_w {
                    let mut ne = *e;
                    ne.start = pos.saturating_add(e.start);
                    if ne.start >= new_w {
                        break;
                    }
                    if ne.start.saturating_add(ne.dur) > new_w {
                        ne.dur = new_w.saturating_sub(ne.start);
                    }
                    ne.gen = self.next_gen;
                    self.next_gen = self.next_gen.wrapping_add(1);
                    let _ = extras.push(ne);
                    let next = pos.saturating_add(tile_end);
                    if next <= pos {
                        break;
                    }
                    pos = next;
                }
            }
            for ne in extras {
                if let Some(slot) = self.events.iter_mut().find(|e| !e.used) {
                    *slot = ne;
                    slot.used = true;
                }
            }
            self.extent = new_w;
        }
        // else: re-extend into preserved region — reveal only.
    }
}

fn note_u8(n: MidiNote) -> u8 {
    u7::from(n).as_int()
}

fn bars_from_fader(v: u16) -> u8 {
    ((v as u32 * (MAX_BARS as u32 - 1)) / 4095) as u8 + 1
}

fn quantize_16th(pos: u16) -> u16 {
    let q = TICKS_PER_16TH as u16;
    (pos / q) * q
}

fn mode_color(mode: usize) -> Color {
    const MODES: [Color; 5] = [
        Color::Cyan,
        Color::Orange,
        Color::Green,
        Color::Blue,
        Color::Pink,
    ];
    MODES[mode.min(4)]
}

fn pitch_from_fader(fader: u16, base: MidiNote, span: i32) -> u8 {
    let semis = (fader as i32 * (span + 3) / 120).clamp(0, 127);
    (note_u8(base) as i32 + semis).clamp(0, 127) as u8
}

#[embassy_executor::task(pool_size = 16/CHANNELS)]
pub async fn wrapper(app: App<CHANNELS>, exit_signal: &'static Signal<NoopRawMutex, bool>) {
    let param_store = ParamStore::<Params>::new(
        app.app_id,
        app.layout_id,
        Params {
            mode: 4,
            midi_channel: MidiChannel::default(),
            midi_in: MidiIn::default(),
            midi_out: MidiOut::default(),
            midi_note: MidiNote::from(48),
            span: 24,
            color: Color::Violet,
            range: Range::_0_10V,
            vpo: VoltPerOct::Standard,
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
    let (mode, midi_chan, midi_in_src, midi_out_dst, base_note, span, _base_color, range, vpo) =
        params.query(|p| {
            (
                p.mode.min(4),
                p.midi_channel,
                p.midi_in,
                p.midi_out,
                p.midi_note,
                p.span,
                p.color,
                p.range,
                p.vpo,
            )
        });

    let led_color = mode_color(mode);
    let needs_cv_in = mode == 0 || mode == 1;
    let needs_cv_out = mode == 3 || mode == 4;
    let uses_button_gate = mode == 0 || mode == 4;

    let buttons = app.use_buttons();
    let faders = app.use_faders();
    let leds = app.use_leds();
    let mut clock = app.use_clock();
    let ticks = clock.get_ticker();
    let quantizer = app.use_quantizer(range, vpo, false);
    let midi = app.use_midi_output(midi_out_dst, midi_chan, false);

    let in_jack: Option<InJack> = if needs_cv_in {
        Some(app.make_in_jack(0, range).await)
    } else {
        None
    };
    let out_jack: Option<OutJack> = if needs_cv_out {
        Some(app.make_out_jack(0, range).await)
    } else {
        None
    };
    if let Some(ref j) = out_jack {
        j.set_value(0);
    }

    let buf = app.make_global(LoopBuf::default());
    let bars_glob = app.make_global(bars_from_fader(storage.query(|s| s.bars_saved)));
    let decay_glob = app.make_global(storage.query(|s| s.decay_saved));
    let main_glob = app.make_global(storage.query(|s| s.main_saved));
    let muted = app.make_global(storage.query(|s| s.muted));
    let armed = app.make_global(storage.query(|s| s.armed));
    let long_press_fired = app.make_global(false);
    let fader_moved_while_held = app.make_global(false);
    let button_down_fader = app.make_global(0u16);
    let glob_latch_layer = app.make_global(LatchLayer::Main);
    let flash_until = app.make_global(0u64);
    let erase_flash = app.make_global(false);
    let live_note = app.make_global(None::<u8>);
    let rec_open = app.make_global(false);
    let voices = app.make_global([(0u8, 0u16, 0u32); POLY]);

    if muted.get() {
        leds.unset(0, Led::Button);
    } else {
        let bright = if armed.get() {
            Brightness::High
        } else {
            Brightness::Low
        };
        leds.set(0, Led::Button, led_color, bright);
    }

    let clock_task = async {
        let mut prev_pos: Option<u16> = None;
        let mut blink_on = true;
        let mut last_blink = Instant::now();

        loop {
            match clock.wait_for_event(ClockDivision::_1).await {
                ClockEvent::Reset | ClockEvent::Stop => {
                    all_notes_off(&midi, &voices, &live_note, out_jack.as_ref()).await;
                    prev_pos = None;
                }
                ClockEvent::Start => {}
                ClockEvent::Tick => {
                    let tick = ticks();
                    let bars = bars_glob.get();
                    let mut b = buf.get();
                    let window = LoopBuf::window_ticks(bars);

                    if let Some(pos) = b.play_pos(tick, bars) {
                        if let Some(pp) = prev_pos {
                            if pos < pp && !muted.get() {
                                b.decay_all(decay_glob.get());
                                buf.set(b);
                                b = buf.get();
                            }
                        }
                        prev_pos = Some(pos);

                        if !muted.get() && b.extent > 0 {
                            for e in b.events.iter() {
                                if !e.used || e.dur == u16::MAX {
                                    continue;
                                }
                                if e.start == pos {
                                    alloc_voice(&midi, &voices, e.note, e.vel, e.gen, e, window)
                                        .await;
                                    flash_until.set(tick.wrapping_add(2));
                                }
                            }

                            let mut vs = voices.get();
                            for v in vs.iter_mut() {
                                if v.0 != 0 && v.1 == pos {
                                    midi.send_note_off(MidiNote::from(v.0)).await;
                                    *v = (0, 0, 0);
                                }
                            }
                            voices.set(vs);

                            if let Some(ref j) = out_jack {
                                update_cv_out(
                                    j,
                                    &quantizer,
                                    &live_note,
                                    &voices,
                                    base_note,
                                    range,
                                    vpo,
                                )
                                .await;
                            }
                        }
                    } else {
                        prev_pos = None;
                    }

                    if last_blink.elapsed().as_millis() >= ARM_BLINK_MS {
                        blink_on = !blink_on;
                        last_blink = Instant::now();
                    }

                    if erase_flash.get() {
                        leds.set(0, Led::Button, Color::White, Brightness::High);
                        erase_flash.set(false);
                    } else if muted.get() {
                        leds.unset(0, Led::Button);
                        leds.unset(0, Led::Top);
                    } else if flash_until.get() > tick {
                        // Black dim/flash on playback hits
                        leds.set(0, Led::Button, Color::Custom(0, 0, 0), Brightness::Low);
                        leds.set(0, Led::Top, Color::Custom(0, 0, 0), Brightness::Low);
                    } else {
                        let bright = if armed.get() {
                            if blink_on {
                                Brightness::High
                            } else {
                                Brightness::Mid
                            }
                        } else {
                            Brightness::Low
                        };
                        leds.set(0, Led::Button, led_color, bright);
                        if armed.get() {
                            leds.set(0, Led::Top, led_color, Brightness::Low);
                        } else {
                            leds.unset(0, Led::Top);
                        }
                    }
                }
            }
        }
    };

    let fader_task = async {
        let mut latch = app.make_latch(faders.get_value());
        let mut last_bars = bars_glob.get();
        loop {
            faders.wait_for_change().await;
            let layer = glob_latch_layer.get();
            let val = faders.get_value();

            if buttons.is_button_pressed(0) {
                let start = button_down_fader.get();
                if start.abs_diff(val) > FADER_MOVE_THRESH {
                    fader_moved_while_held.set(true);
                }
            }

            let target = match layer {
                LatchLayer::Main => storage.query(|s| s.main_saved),
                LatchLayer::Alt => storage.query(|s| s.decay_saved),
                LatchLayer::Third => storage.query(|s| s.bars_saved),
            };

            if let Some(new_value) = latch.update(val, layer, target) {
                match layer {
                    LatchLayer::Main => {
                        main_glob.set(new_value);
                        storage.modify_and_save(|s| s.main_saved = new_value);
                    }
                    LatchLayer::Alt => {
                        decay_glob.set(new_value);
                        storage.modify_and_save(|s| s.decay_saved = new_value);
                        leds.set(
                            0,
                            Led::Top,
                            Color::Red,
                            Brightness::Custom((new_value / 16) as u8),
                        );
                    }
                    LatchLayer::Third => {
                        let new_bars = bars_from_fader(new_value);
                        let mut b = buf.get();
                        b.set_window_bars(last_bars, new_bars);
                        buf.set(b);
                        bars_glob.set(new_bars);
                        last_bars = new_bars;
                        storage.modify_and_save(|s| s.bars_saved = new_value);
                        leds.set(
                            0,
                            Led::Bottom,
                            Color::White,
                            Brightness::Custom((new_bars as u16 * 28) as u8),
                        );
                    }
                }
            }
        }
    };

    let controls = async {
        loop {
            buttons.wait_for_down(0).await;
            let shift = buttons.is_shift_pressed();
            long_press_fired.set(false);
            fader_moved_while_held.set(false);
            button_down_fader.set(faders.get_value());

            if shift {
                match select(buttons.wait_for_up(0), buttons.wait_for_any_long_press()).await {
                    Either::First(_) => {
                        if !long_press_fired.get() {
                            let a = armed.toggle();
                            storage.modify_and_save(|s| s.armed = a);
                        }
                    }
                    Either::Second(_) => {
                        long_press_fired.set(true);
                        let mut b = buf.get();
                        b.clear();
                        buf.set(b);
                        all_notes_off(&midi, &voices, &live_note, out_jack.as_ref()).await;
                        erase_flash.set(true);
                        buttons.wait_for_up(0).await;
                    }
                }
                continue;
            }

            if uses_button_gate && !muted.get() {
                let note = resolve_pitch(
                    mode,
                    &quantizer,
                    in_jack.as_ref(),
                    main_glob.get(),
                    base_note,
                    span,
                )
                .await;
                start_note(
                    &midi,
                    &buf,
                    &bars_glob,
                    &armed,
                    &live_note,
                    &rec_open,
                    ticks,
                    note,
                    4095,
                    out_jack.as_ref(),
                    range,
                    vpo,
                    &quantizer,
                    needs_cv_out,
                )
                .await;
            }

            match select(buttons.wait_for_up(0), buttons.wait_for_any_long_press()).await {
                Either::First(_) => {
                    if !long_press_fired.get() && uses_button_gate {
                        note_off(
                            &midi,
                            &buf,
                            &bars_glob,
                            &live_note,
                            &rec_open,
                            &armed,
                            ticks,
                            out_jack.as_ref(),
                        )
                        .await;
                    }
                }
                Either::Second(_) => {
                    long_press_fired.set(true);
                    if !fader_moved_while_held.get() {
                        if let Some(n) = live_note.get() {
                            midi.send_note_off(MidiNote::from(n)).await;
                            live_note.set(None);
                            rec_open.set(false);
                        }
                        let m = muted.toggle();
                        storage.modify_and_save(|s| s.muted = m);
                        if m {
                            all_notes_off(&midi, &voices, &live_note, out_jack.as_ref()).await;
                        }
                    } else if uses_button_gate {
                        buttons.wait_for_up(0).await;
                        note_off(
                            &midi,
                            &buf,
                            &bars_glob,
                            &live_note,
                            &rec_open,
                            &armed,
                            ticks,
                            out_jack.as_ref(),
                        )
                        .await;
                        continue;
                    }
                    buttons.wait_for_up(0).await;
                }
            }
        }
    };

    let input_task = async {
        match mode {
            1 => {
                let jack = in_jack.as_ref().unwrap();
                let mut old = jack.get_value();
                loop {
                    app.delay_millis(1).await;
                    let v = jack.get_value();
                    if muted.get() {
                        old = v;
                        continue;
                    }
                    if v >= GATE_THRESH && old < GATE_THRESH {
                        let note = pitch_from_fader(main_glob.get(), base_note, span);
                        start_note(
                            &midi,
                            &buf,
                            &bars_glob,
                            &armed,
                            &live_note,
                            &rec_open,
                            ticks,
                            note,
                            4095,
                            out_jack.as_ref(),
                            range,
                            vpo,
                            &quantizer,
                            false,
                        )
                        .await;
                    } else if v < GATE_THRESH && old >= GATE_THRESH {
                        note_off(
                            &midi,
                            &buf,
                            &bars_glob,
                            &live_note,
                            &rec_open,
                            &armed,
                            ticks,
                            out_jack.as_ref(),
                        )
                        .await;
                    }
                    old = v;
                }
            }
            2 | 3 => {
                let mut midi_in = app.use_midi_input(midi_in_src, midi_chan);
                loop {
                    match midi_in.wait_for_event().await {
                        AppMidiEvent::Message(MidiMessage::NoteOn { key, vel }) => {
                            if muted.get() {
                                continue;
                            }
                            let n = u8::from(key);
                            if u8::from(vel) == 0 {
                                end_specific(
                                    &midi,
                                    &buf,
                                    &bars_glob,
                                    &live_note,
                                    &rec_open,
                                    &armed,
                                    ticks,
                                    n,
                                    out_jack.as_ref(),
                                )
                                .await;
                            } else {
                                let v12 = ((u8::from(vel) as u32 * 4095) / 127) as u16;
                                start_note(
                                    &midi,
                                    &buf,
                                    &bars_glob,
                                    &armed,
                                    &live_note,
                                    &rec_open,
                                    ticks,
                                    n,
                                    v12.max(KILL_FLOOR + 1),
                                    out_jack.as_ref(),
                                    range,
                                    vpo,
                                    &quantizer,
                                    mode == 3,
                                )
                                .await;
                            }
                        }
                        AppMidiEvent::Message(MidiMessage::NoteOff { key, .. }) => {
                            end_specific(
                                &midi,
                                &buf,
                                &bars_glob,
                                &live_note,
                                &rec_open,
                                &armed,
                                ticks,
                                u8::from(key),
                                out_jack.as_ref(),
                            )
                            .await;
                        }
                        _ => {}
                    }
                }
            }
            _ => loop {
                app.delay_millis(1000).await;
            },
        }
    };

    let layer_task = async {
        loop {
            app.delay_millis(1).await;
            // When the button is the play/gate key, keep Main so fader = pitch.
            // Third (bars) only while muted, or in modes where button isn't gate.
            let layer = if buttons.is_shift_pressed() && !buttons.is_button_pressed(0) {
                LatchLayer::Alt
            } else if !buttons.is_shift_pressed()
                && buttons.is_button_pressed(0)
                && (muted.get() || !uses_button_gate)
            {
                LatchLayer::Third
            } else {
                LatchLayer::Main
            };
            glob_latch_layer.set(layer);
        }
    };

    let scene_task = async {
        loop {
            match app.wait_for_scene_event().await {
                SceneEvent::LoadScene(scene) => {
                    storage.load_from_scene(scene).await;
                    muted.set(storage.query(|s| s.muted));
                    armed.set(storage.query(|s| s.armed));
                    decay_glob.set(storage.query(|s| s.decay_saved));
                    main_glob.set(storage.query(|s| s.main_saved));
                    bars_glob.set(bars_from_fader(storage.query(|s| s.bars_saved)));
                }
                SceneEvent::SaveScene(scene) => {
                    storage.save_to_scene(scene).await;
                }
            }
        }
    };

    join5(
        clock_task,
        fader_task,
        controls,
        select(input_task, layer_task),
        scene_task,
    )
    .await;
}

async fn resolve_pitch(
    mode: usize,
    quantizer: &Quantizer,
    in_jack: Option<&InJack>,
    main: u16,
    base_note: MidiNote,
    span: i32,
) -> u8 {
    if mode == 0 {
        if let Some(j) = in_jack {
            let pitch = quantizer.get_quantized_note(j.get_value()).await;
            return (note_u8(base_note) as i32 + note_u8(pitch.as_midi()) as i32).clamp(0, 127)
                as u8;
        }
    }
    pitch_from_fader(main, base_note, span)
}

async fn all_notes_off(
    midi: &MidiOutput,
    voices: &Global<[(u8, u16, u32); POLY]>,
    live_note: &Global<Option<u8>>,
    out_jack: Option<&OutJack>,
) {
    let vs = voices.get();
    for (n, _, _) in vs {
        if n > 0 {
            midi.send_note_off(MidiNote::from(n)).await;
        }
    }
    voices.set([(0, 0, 0); POLY]);
    if let Some(n) = live_note.get() {
        midi.send_note_off(MidiNote::from(n)).await;
        live_note.set(None);
    }
    if let Some(j) = out_jack {
        j.set_value(0);
    }
}

async fn alloc_voice(
    midi: &MidiOutput,
    voices: &Global<[(u8, u16, u32); POLY]>,
    note: u8,
    vel: u16,
    gen: u32,
    e: &NoteEvent,
    window: u16,
) {
    let end = e.start.saturating_add(e.dur) % window.max(1);
    let mut vs = voices.get();
    for v in vs.iter_mut() {
        if v.0 == note {
            midi.send_note_off(MidiNote::from(note)).await;
            *v = (note, end, gen);
            midi.send_note_on(MidiNote::from(note), vel).await;
            voices.set(vs);
            return;
        }
    }
    if let Some(slot) = vs.iter_mut().find(|v| v.0 == 0) {
        *slot = (note, end, gen);
        midi.send_note_on(MidiNote::from(note), vel).await;
        voices.set(vs);
        return;
    }
    let mut oi = 0;
    let mut og = u32::MAX;
    for (i, v) in vs.iter().enumerate() {
        if v.2 < og {
            og = v.2;
            oi = i;
        }
    }
    midi.send_note_off(MidiNote::from(vs[oi].0)).await;
    vs[oi] = (note, end, gen);
    midi.send_note_on(MidiNote::from(note), vel).await;
    voices.set(vs);
}

async fn update_cv_out(
    j: &OutJack,
    quantizer: &Quantizer,
    live_note: &Global<Option<u8>>,
    voices: &Global<[(u8, u16, u32); POLY]>,
    base_note: MidiNote,
    range: Range,
    vpo: VoltPerOct,
) {
    let vs = voices.get();
    let note = live_note
        .get()
        .or_else(|| vs.iter().find(|v| v.0 != 0).map(|v| v.0));
    if let Some(n) = note {
        let base = note_u8(base_note);
        let rel = (n as i32 - base as i32).clamp(0, 120);
        let counts = ((rel as u32 * 4095) / 120) as u16;
        let pitch = quantizer.get_quantized_note(counts).await;
        j.set_value(pitch.as_counts(range, vpo));
    } else {
        j.set_value(0);
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_note(
    midi: &MidiOutput,
    buf: &Global<LoopBuf>,
    bars_glob: &Global<u8>,
    armed: &Global<bool>,
    live_note: &Global<Option<u8>>,
    rec_open: &Global<bool>,
    ticks: fn() -> u64,
    note: u8,
    vel: u16,
    out_jack: Option<&OutJack>,
    range: Range,
    vpo: VoltPerOct,
    quantizer: &Quantizer,
    cv_out: bool,
) {
    if let Some(old) = live_note.get() {
        midi.send_note_off(MidiNote::from(old)).await;
    }
    live_note.set(Some(note));
    midi.send_note_on(MidiNote::from(note), vel).await;

    if cv_out {
        if let Some(j) = out_jack {
            let counts = ((note as u32 * 4095) / 127) as u16;
            let pitch = quantizer.get_quantized_note(counts).await;
            j.set_value(pitch.as_counts(range, vpo));
        }
    }

    if armed.get() {
        let tick = ticks();
        let bars = bars_glob.get();
        let window = LoopBuf::window_ticks(bars);
        let mut b = buf.get();
        b.ensure_origin(tick);
        let pos = b.play_pos(tick, bars).unwrap_or(0);
        let qpos = quantize_16th(pos) % window.max(1);
        b.add_or_refresh(qpos, u16::MAX, note, vel, window);
        buf.set(b);
        rec_open.set(true);
    }
}

#[allow(clippy::too_many_arguments)]
async fn note_off(
    midi: &MidiOutput,
    buf: &Global<LoopBuf>,
    bars_glob: &Global<u8>,
    live_note: &Global<Option<u8>>,
    rec_open: &Global<bool>,
    armed: &Global<bool>,
    ticks: fn() -> u64,
    out_jack: Option<&OutJack>,
) {
    if let Some(n) = live_note.get() {
        end_specific(
            midi, buf, bars_glob, live_note, rec_open, armed, ticks, n, out_jack,
        )
        .await;
    } else if let Some(j) = out_jack {
        j.set_value(0);
    }
}

#[allow(clippy::too_many_arguments)]
async fn end_specific(
    midi: &MidiOutput,
    buf: &Global<LoopBuf>,
    bars_glob: &Global<u8>,
    live_note: &Global<Option<u8>>,
    rec_open: &Global<bool>,
    armed: &Global<bool>,
    ticks: fn() -> u64,
    note: u8,
    out_jack: Option<&OutJack>,
) {
    midi.send_note_off(MidiNote::from(note)).await;
    if live_note.get() == Some(note) {
        live_note.set(None);
        if let Some(j) = out_jack {
            j.set_value(0);
        }
    }
    if armed.get() && rec_open.get() {
        let tick = ticks();
        let bars = bars_glob.get();
        let window = LoopBuf::window_ticks(bars);
        let mut b = buf.get();
        let pos = b.play_pos(tick, bars).unwrap_or(0);
        let qpos = quantize_16th(pos) % window.max(1);
        b.finalize_open(note, qpos, window);
        for e in b.events.iter_mut() {
            if e.used && e.note == note && e.dur == u16::MAX {
                e.dur = DEFAULT_GATE_TICKS;
            }
        }
        buf.set(b);
        rec_open.set(false);
    }
}
