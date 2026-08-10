use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    mutex::Mutex,
    signal::Signal,
    watch::Watch,
};
use embassy_time::Timer;
use portable_atomic::{AtomicU32, AtomicU8, Ordering};
use static_cell::StaticCell;

use libfp::{Brightness, Color, InnerLayout, Layout, GLOBAL_CHANNELS};

use crate::app::Led;
use crate::apps::spawn_app_by_id;
use crate::tasks::leds::{set_led_mode, LedMode, LedMsg};
use crate::tasks::midi::{
    arm_post_layout_perf_mute, host_holds_perf_mute, set_layout_spawn_active,
    set_layout_usb_midi_mute, set_spawn_start_held, POST_LAYOUT_PERF_MUTE_MS,
};

// Receivers: layout spawn loop, configure
const LAYOUT_WATCH_SUBSCRIBERS: usize = 2;

pub static LAYOUT_WATCH: Watch<CriticalSectionRawMutex, Layout, LAYOUT_WATCH_SUBSCRIBERS> =
    Watch::new();

/// Signal to force respawn all apps
pub static FORCE_RESPAWN_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// A scoped, non-persisting request to evict or restore a single channel's
/// app, used by the V/Oct calibration wizard to temporarily free a jack an
/// app is using without touching the persisted layout.
pub enum EvictionCmd {
    /// Exit whatever app is running on this start_channel, if any.
    Evict(usize),
    /// Respawn (app_id, channels, layout_id) on this start_channel.
    Restore(usize, u8, usize, u8),
}

pub static LAYOUT_EVICTION_REQ: Signal<CriticalSectionRawMutex, EvictionCmd> = Signal::new();
pub static LAYOUT_EVICTION_RES: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Core0 ReleasePerfMute → Core1: persist the layout and clear HoldPerfMute.
pub static RELEASE_SPAWN_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Core1 → Core0: ReleasePerfMute handling finished.
pub static RELEASE_SPAWN_DONE: Signal<CriticalSectionRawMutex, bool> = Signal::new();

pub static LAYOUT_MANAGER: StaticCell<LayoutManager> = StaticCell::new();

/// Breadcrumb for RTT / GetVersion logging if USB dies mid-SetLayout.
/// 0=idle 1=mute 2=exit 3=spawn 4=store 5=done
pub static LAYOUT_DEBUG_PHASE: AtomicU8 = AtomicU8::new(0);
pub static LAYOUT_DEBUG_APP_ID: AtomicU8 = AtomicU8::new(0);
pub static LAYOUT_DEBUG_CHANNEL: AtomicU8 = AtomicU8::new(0);
pub static LAYOUT_DEBUG_SEQ: AtomicU32 = AtomicU32::new(0);

fn layout_debug(phase: u8, app_id: u8, channel: usize) {
    LAYOUT_DEBUG_PHASE.store(phase, Ordering::Relaxed);
    LAYOUT_DEBUG_APP_ID.store(app_id, Ordering::Relaxed);
    LAYOUT_DEBUG_CHANNEL.store(channel as u8, Ordering::Relaxed);
    LAYOUT_DEBUG_SEQ.fetch_add(1, Ordering::Relaxed);
    defmt::info!(
        "layout dbg phase={} app={} ch={} seq={}",
        phase,
        app_id,
        channel,
        LAYOUT_DEBUG_SEQ.load(Ordering::Relaxed)
    );
}

fn layout_debug_led(channel: usize, color: Color) {
    if channel < GLOBAL_CHANNELS {
        set_led_mode(
            channel,
            Led::Button,
            LedMsg::Set(LedMode::Static(color, Brightness::High)),
        );
    }
}

/// Gap between successive app spawns so USB MIDI stays responsive.
const SPAWN_STAGGER_MS: u64 = 250;
const SPAWN_STAGGER_DENSE_MS: u64 = 500;
const SPAWN_STAGGER_VERY_DENSE_MS: u64 = 800;
/// Paced spawn under HoldPerfMute (host Full Push). Longer gaps once many
/// tasks are already live — dense layouts stress Core0/USB and the task arena.
const SPAWN_STAGGER_RELEASE_MS: u64 = 700;
const SPAWN_STAGGER_RELEASE_DENSE_MS: u64 = 1100;
const SPAWN_STAGGER_RELEASE_TAIL_MS: u64 = 1600;
/// Extra quiet every N successful Release spawns.
const SPAWN_RELEASE_BREATH_EVERY: usize = 3;
const SPAWN_RELEASE_BREATH_MS: u64 = 2000;
const SPAWN_DENSE_THRESHOLD: usize = 6;
const SPAWN_VERY_DENSE_THRESHOLD: usize = 10;

pub struct LayoutManager {
    exit_signals: [Signal<NoopRawMutex, bool>; GLOBAL_CHANNELS],
    layout: Mutex<NoopRawMutex, InnerLayout>,
    /// Channels currently on loan for V/Oct calibration (see `EvictionCmd`).
    /// `spawn_layout`'s reconciliation pass must not spawn into a held
    /// channel even if the persisted layout wants an app there, since a
    /// held channel is mid-calibration and not actually free.
    held: Mutex<NoopRawMutex, [bool; GLOBAL_CHANNELS]>,
    spawner: Spawner,
}

impl LayoutManager {
    pub fn new(spawner: Spawner) -> Self {
        Self {
            exit_signals: [const { Signal::new() }; GLOBAL_CHANNELS],
            layout: Mutex::new([None; GLOBAL_CHANNELS]),
            held: Mutex::new([false; GLOBAL_CHANNELS]),
            spawner,
        }
    }

    /// Mark `start_channel` as held (or release it) for a temporary V/Oct
    /// calibration eviction, so ordinary layout reconciliation leaves it
    /// alone until it's released.
    pub(crate) async fn set_held(&self, start_channel: usize, held: bool) {
        self.held.lock().await[start_channel] = held;
    }

    pub(crate) async fn exit_app(&self, start_channel: usize) {
        let mut layout = self.layout.lock().await;
        if let Some((app_id, _, _)) = layout[start_channel] {
            layout[start_channel] = None;
            drop(layout);

            layout_debug(2, app_id, start_channel);
            layout_debug_led(start_channel, Color::Red);

            self.exit_signals[start_channel].signal(true);
            Timer::after_millis(120).await;
            // Stale signal would make the next task on this channel exit immediately.
            self.exit_signals[start_channel].reset();
        }
    }

    /// Force respawn all apps by exiting them all and then respawning with the given layout
    pub async fn respawn_all(&'static self, layout: &Layout) {
        set_layout_usb_midi_mute(true);
        layout_debug(1, 0, 0);
        for start_channel in 0..GLOBAL_CHANNELS {
            self.exit_app(start_channel).await;
        }
        Timer::after_millis(500).await;
        self.spawn_layout(layout).await;
    }

    /// Spawn a single (app_id, channels, layout_id) onto `start_channel` if
    /// nothing is currently running there. Used to restore an app that was
    /// temporarily evicted (e.g. for V/Oct calibration) without touching the
    /// persisted layout.
    pub(crate) async fn spawn_one(
        &'static self,
        start_channel: usize,
        app_id: u8,
        channels: usize,
        layout_id: u8,
    ) {
        let mut current_layout = self.layout.lock().await;
        if current_layout[start_channel].is_none() {
            spawn_app_by_id(
                app_id,
                start_channel,
                layout_id,
                self.spawner,
                &self.exit_signals,
            );
            current_layout[start_channel] = Some((app_id, channels, layout_id));
        }
    }

    /// Apply layout. HoldPerfMute only mutes Local MIDI — SetLayout still spawns
    /// (paced). Deferred-until-Release multi-app burst wedged USB on every Full
    /// Push; hosts must add one app per SetLayout under Hold instead.
    pub async fn spawn_layout(&'static self, layout: &Layout) -> bool {
        let mut changed = false;
        set_layout_spawn_active(true);
        set_layout_usb_midi_mute(true);
        layout_debug(1, 0, 0);
        defmt::info!("layout spawn_layout begin hold={}", host_holds_perf_mute());

        let mut desired_layout: InnerLayout = [None; GLOBAL_CHANNELS];
        for (app_id, start_channel, channels, layout_id) in layout.iter() {
            if start_channel < GLOBAL_CHANNELS {
                desired_layout[start_channel] = Some((app_id, channels, layout_id));
            }
        }

        // Pass 1: Exit apps that are no longer desired or are different
        for start_channel in 0..GLOBAL_CHANNELS {
            let current_app = {
                let current_layout = self.layout.lock().await;
                current_layout[start_channel]
            };

            let desired_app = desired_layout[start_channel];

            match (current_app, desired_app) {
                (
                    Some((cur_id, cur_channels, cur_layout_id)),
                    Some((des_id, des_channels, des_layout_id)),
                ) => {
                    if cur_id != des_id
                        || cur_channels != des_channels
                        || cur_layout_id != des_layout_id
                    {
                        self.exit_app(start_channel).await;
                        changed = true;
                    }
                }
                (Some(_), None) => {
                    self.exit_app(start_channel).await;
                    changed = true;
                }
                _ => {}
            }
        }

        if changed {
            // Let Embassy task-arena slots drop before spawning replacements.
            Timer::after_millis(400).await;
        }

        // Pass 2: Spawn apps. Multi-app Pass 2 under Hold uses the start gate;
        // single-app incremental SetLayout skips it so param_handler is up ASAP.
        let release_pace = host_holds_perf_mute();
        let mut need_spawn = 0usize;
        {
            let current_layout = self.layout.lock().await;
            for start_channel in 0..GLOBAL_CHANNELS {
                if current_layout[start_channel].is_none()
                    && desired_layout[start_channel].is_some()
                {
                    need_spawn += 1;
                }
            }
        }
        let use_start_gate = release_pace && need_spawn > 1;
        if use_start_gate {
            Timer::after_millis(400).await;
            set_spawn_start_held(true);
            defmt::info!("layout spawn-start gate CLOSED ({} pending)", need_spawn);
        }
        set_layout_spawn_active(false);
        for start_channel in 0..GLOBAL_CHANNELS {
            let current_app = {
                let current_layout = self.layout.lock().await;
                current_layout[start_channel]
            };

            if current_app.is_some() {
                continue;
            }

            if self.held.lock().await[start_channel] {
                continue;
            }

            if let Some((app_id, channels, layout_id)) = desired_layout[start_channel] {
                layout_debug(3, app_id, start_channel);
                layout_debug_led(start_channel, Color::Yellow);
                defmt::info!(
                    "layout spawn app_id={} ch={} layout_id={}",
                    app_id,
                    start_channel,
                    layout_id
                );

                set_layout_spawn_active(true);
                let spawned = spawn_app_by_id(
                    app_id,
                    start_channel,
                    layout_id,
                    self.spawner,
                    &self.exit_signals,
                );
                set_layout_spawn_active(false);

                match spawned {
                    true => {
                        layout_debug(4, app_id, start_channel);
                        layout_debug_led(start_channel, Color::Green);
                        let running = {
                            let mut current_layout = self.layout.lock().await;
                            current_layout[start_channel] = Some((app_id, channels, layout_id));
                            current_layout.iter().filter(|s| s.is_some()).count()
                        };
                        changed = true;
                        let stagger = if release_pace {
                            if running >= 8 {
                                SPAWN_STAGGER_RELEASE_TAIL_MS
                            } else if running >= SPAWN_DENSE_THRESHOLD {
                                SPAWN_STAGGER_RELEASE_DENSE_MS
                            } else {
                                SPAWN_STAGGER_RELEASE_MS
                            }
                        } else if running >= SPAWN_VERY_DENSE_THRESHOLD {
                            SPAWN_STAGGER_VERY_DENSE_MS
                        } else if running >= SPAWN_DENSE_THRESHOLD {
                            SPAWN_STAGGER_DENSE_MS
                        } else {
                            SPAWN_STAGGER_MS
                        };
                        Timer::after_millis(stagger).await;
                        if release_pace
                            && running >= SPAWN_RELEASE_BREATH_EVERY
                            && running.is_multiple_of(SPAWN_RELEASE_BREATH_EVERY)
                        {
                            defmt::info!("layout spawn breath after {}", running);
                            Timer::after_millis(SPAWN_RELEASE_BREATH_MS).await;
                        }
                    }
                    false => {
                        defmt::error!(
                            "Failed to spawn app_id={} at channel {}",
                            app_id,
                            start_channel
                        );
                        layout_debug_led(start_channel, Color::Red);
                        // Don't burst-retry into an exhausted TaskPool —
                        // same quiet gap as a successful spawn.
                        Timer::after_millis(if release_pace {
                            SPAWN_STAGGER_RELEASE_MS
                        } else {
                            SPAWN_STAGGER_MS
                        })
                        .await;
                    }
                }
            }
        }

        if use_start_gate {
            Timer::after_millis(200).await;
            set_spawn_start_held(false);
            defmt::info!("layout spawn-start gate OPEN");
            Timer::after_millis(600).await;
        }

        layout_debug(5, 0, 0);
        let running = {
            let current_layout = self.layout.lock().await;
            current_layout.iter().filter(|s| s.is_some()).count()
        };
        if release_pace {
            Timer::after_millis(400).await;
        } else if running >= SPAWN_VERY_DENSE_THRESHOLD {
            Timer::after_millis(800).await;
        } else if running >= SPAWN_DENSE_THRESHOLD {
            Timer::after_millis(400).await;
        }

        set_layout_spawn_active(false);
        if release_pace {
            // Keep hard mute for the whole Hold; editor Release clears it.
            defmt::info!(
                "layout spawn_layout end under_hold running={} changed={}",
                running,
                changed
            );
        } else {
            set_layout_usb_midi_mute(false);
            if changed {
                arm_post_layout_perf_mute(POST_LAYOUT_PERF_MUTE_MS);
            }
            defmt::info!(
                "layout spawn_layout end changed={} soft_mute_ms={}",
                changed,
                if changed { POST_LAYOUT_PERF_MUTE_MS } else { 0 }
            );
        }
        changed
    }
}
