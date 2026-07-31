use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use heapless::Vec;
use postcard::{from_bytes, to_slice};

use libfp::sysex::{
    pack_7bit, unpack_7bit, MAX_PLAIN_SIZE, MAX_SYSEX_FRAME, SYSEX_EOX, SYSEX_HEADER, SYSEX_START,
};
use libfp::{ConfigMsgIn, ConfigMsgOut, Value, APP_MAX_PARAMS, GLOBAL_CHANNELS};

use crate::app::Led;
use crate::apps::{get_channels, get_config, REGISTERED_APP_IDS};
use crate::layout::{
    LAYOUT_DEBUG_APP_ID, LAYOUT_DEBUG_CHANNEL, LAYOUT_DEBUG_PHASE, LAYOUT_DEBUG_SEQ, LAYOUT_WATCH,
    RELEASE_SPAWN_DONE, RELEASE_SPAWN_SIGNAL,
};
use crate::storage::factory_reset;
use crate::tasks::global_config::{get_global_config, GLOBAL_CONFIG_WATCH};
use crate::tasks::leds::{set_led_mode, LedMode, LedMsg};
use crate::tasks::midi::{
    extend_post_layout_perf_mute, layout_spawn_active, set_config_holds_perf_usb, spawn_start_held,
    SharedUsbSender, CONFIG_SOFT_MUTE_EXTEND_MS, PERF_CABLE,
};
use crate::version::FIRMWARE_VERSION;
use libfp::Color;
use portable_atomic::Ordering;

use super::transport::USB_MAX_PACKET_SIZE;

/// Buffer size for one reassembled config SysEx frame body (header + packed
/// payload, without F0/F7). Slightly above MAX_SYSEX_FRAME for headroom.
pub const CONFIG_FRAME_BUF: usize = 640;

/// Complete config SysEx frame bodies from the USB MIDI RX path
/// (tasks/midi.rs). Depth >1 so a burst during SetLayout cannot drop the
/// next host request (GetVersion) while configure is still finishing ACK TX.
pub static CONFIG_RX_CHANNEL: Channel<CriticalSectionRawMutex, Vec<u8, CONFIG_FRAME_BUF>, 4> =
    Channel::new();

/// Per-packet write timeout for config responses. Generous compared to the
/// 1ms performance-MIDI timeout: config frames must not be silently
/// truncated, but a stalled host must not block the USB sender forever.
const CONFIG_WRITE_TIMEOUT_MS: u64 = 500;
/// Per-app SetAppParams timeout (FRAM save under load).
const APP_PARAM_SET_TIMEOUT_MS: u64 = 8000;
/// GetAppParams / readiness: fail faster with empty AppState so hosts
/// (poll timeout ~10s) actually see a reply instead of cable silence.
const APP_PARAM_GET_TIMEOUT_MS: u64 = 3000;

pub enum AppParamCmd {
    SetAppParams {
        values: [Option<Value>; APP_MAX_PARAMS],
    },
    RequestParamValues,
}

pub static APP_PARAM_SIGNALS: [Signal<CriticalSectionRawMutex, AppParamCmd>; GLOBAL_CHANNELS] =
    [const { Signal::new() }; GLOBAL_CHANNELS];

pub static APP_PARAM_CHANNEL: Channel<
    CriticalSectionRawMutex,
    (u8, Vec<Value, APP_MAX_PARAMS>),
    GLOBAL_CHANNELS,
> = Channel::new();

/// Unsolicited app→host param updates (e.g. Shift+fader genre pick on the
/// device). Forwarded as AppState while the config loop is idle, so hosts
/// (configurator / Scopepunk) track on-device edits live. Separate from
/// APP_PARAM_CHANNEL, which carries request/response replies only.
pub static APP_PARAM_PUSH_CHANNEL: Channel<
    CriticalSectionRawMutex,
    (u8, Vec<Value, APP_MAX_PARAMS>),
    4,
> = Channel::new();

/// Drop stale replies left by a previous Set/Get (shared channel is not
/// per-layout). Without this, GetAppParams(N) can ack with AppState(N-1).
fn drain_app_param_channel() {
    while APP_PARAM_CHANNEL.try_receive().is_ok() {}
}

/// Wait for the matching layout_id on the shared param channel.
async fn recv_app_params_for(
    layout_id: u8,
    timeout_ms: u64,
) -> Option<(u8, Vec<Value, APP_MAX_PARAMS>)> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match with_timeout(remaining, APP_PARAM_CHANNEL.receive()).await {
            Ok((id, values)) if id == layout_id => return Some((id, values)),
            Ok((id, _)) => {
                defmt::warn!(
                    "Ignoring AppParam reply layout_id={} (want {})",
                    id,
                    layout_id
                );
            }
            Err(_) => return None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum ProtocolError {
    BufferTooSmall,
    DecodingError,
    EncodingError,
    TransmissionError,
    CorruptedMessage,
    Timeout,
}

pub async fn start_config_loop<'a>(usb_tx: &'a SharedUsbSender<'a>) {
    let mut proto = ConfigTransport::new(usb_tx);
    let mut layout_receiver = LAYOUT_WATCH.receiver().unwrap();
    let mut layout = layout_receiver.get().await;
    loop {
        // While idle, also forward unsolicited on-device param edits (genre
        // scrub etc.) as AppState. Cancel-safe: read_msg awaits on the frame
        // channel; no partial frame state is lost when the select flips.
        let sel =
            embassy_futures::select::select(proto.read_msg(), APP_PARAM_PUSH_CHANNEL.receive())
                .await;
        let msg = match sel {
            embassy_futures::select::Either::First(Ok(msg)) => msg,
            embassy_futures::select::Either::First(Err(err)) => {
                defmt::warn!("Dropping invalid config frame: {}", err);
                continue;
            }
            embassy_futures::select::Either::Second((id, values)) => {
                if let Err(err) = proto.send_msg(ConfigMsgOut::AppState(id, &values)).await {
                    defmt::warn!("Failed to push AppState({}): {}", id, err);
                }
                continue;
            }
        };
        // Only mutating layout/param traffic should keep Local MIDI muted.
        // Extending on every Get* / SetGlobalConfig made Scopepunk Start
        // (GetGlobalConfig ± clock-src write) silence apps for ~8s ≈ 200+
        // MIDI clocks at typical BPM before any notes appeared.
        match &msg {
            ConfigMsgIn::SetLayout(_)
            | ConfigMsgIn::SetAppParams { .. }
            | ConfigMsgIn::HoldPerfMute
            | ConfigMsgIn::FactoryReset => {
                extend_post_layout_perf_mute(CONFIG_SOFT_MUTE_EXTEND_MS);
            }
            _ => {}
        }
        set_config_holds_perf_usb(true);
        let res = match msg {
            ConfigMsgIn::Ping => proto.send_msg(ConfigMsgOut::Pong).await,
            ConfigMsgIn::GetVersion => {
                // Layout breadcrumb: last SetLayout phase survives a USB wedge
                // and shows up on the next successful GetVersion via RTT.
                defmt::info!(
                    "cfg GetVersion layout_dbg phase={} app={} ch={} seq={}",
                    LAYOUT_DEBUG_PHASE.load(Ordering::Relaxed),
                    LAYOUT_DEBUG_APP_ID.load(Ordering::Relaxed),
                    LAYOUT_DEBUG_CHANNEL.load(Ordering::Relaxed),
                    LAYOUT_DEBUG_SEQ.load(Ordering::Relaxed)
                );
                let (major, minor, patch) = FIRMWARE_VERSION;
                proto
                    .send_msg(ConfigMsgOut::Version {
                        major,
                        minor,
                        patch,
                    })
                    .await
            }
            ConfigMsgIn::GetAllApps => {
                let configs = REGISTERED_APP_IDS.map(get_config);
                let mut res = proto
                    .send_msg(ConfigMsgOut::BatchMsgStart(configs.len()))
                    .await;
                for (app_id, channels, config_meta) in configs.into_iter().flatten() {
                    if res.is_err() {
                        break;
                    }
                    res = proto
                        .send_msg(ConfigMsgOut::AppConfig(app_id, channels, config_meta))
                        .await;
                }
                if res.is_ok() {
                    res = proto.send_msg(ConfigMsgOut::BatchMsgEnd).await;
                }
                res
            }
            ConfigMsgIn::GetLayout => proto.send_msg(ConfigMsgOut::Layout(layout.clone())).await,
            ConfigMsgIn::GetGlobalConfig => {
                let config = get_global_config();
                proto.send_msg(ConfigMsgOut::GlobalConfig(config)).await
            }
            ConfigMsgIn::GetAppParams { layout_id } => {
                // During spawn/teardown / start-gate, apps have no param_handler
                // yet — waiting would block the config loop past the host timeout
                // and desync SysEx (host gives up, FW still holding the reply).
                if layout_spawn_active() || spawn_start_held() {
                    proto.send_msg(ConfigMsgOut::AppState(layout_id, &[])).await
                } else {
                    drain_app_param_channel();
                    APP_PARAM_SIGNALS[layout_id as usize].signal(AppParamCmd::RequestParamValues);
                    // Under Hold, fail fast — empty AppState lets the editor poll.
                    let wait_ms = if crate::tasks::midi::host_holds_perf_mute() {
                        400
                    } else {
                        APP_PARAM_GET_TIMEOUT_MS
                    };
                    if let Some((res_layout_id, values)) =
                        recv_app_params_for(layout_id, wait_ms).await
                    {
                        proto
                            .send_msg(ConfigMsgOut::AppState(res_layout_id, &values))
                            .await
                    } else {
                        defmt::warn!("GetAppParams layout_id={} timed out", layout_id);
                        proto.send_msg(ConfigMsgOut::AppState(layout_id, &[])).await
                    }
                }
            }
            ConfigMsgIn::SetAppParams { layout_id, values } => {
                if layout_spawn_active() || spawn_start_held() {
                    let _ = values;
                    proto.send_msg(ConfigMsgOut::AppState(layout_id, &[])).await
                } else {
                    drain_app_param_channel();
                    APP_PARAM_SIGNALS[layout_id as usize]
                        .signal(AppParamCmd::SetAppParams { values });
                    if let Some((res_layout_id, values)) =
                        recv_app_params_for(layout_id, APP_PARAM_SET_TIMEOUT_MS).await
                    {
                        // Confirm to the user which slot just took params.
                        for (_app_id, start_channel, _channels, lid) in layout.iter() {
                            if lid == res_layout_id {
                                set_led_mode(
                                    start_channel,
                                    Led::Button,
                                    LedMsg::Set(LedMode::Flash(Color::Green, Some(2))),
                                );
                                break;
                            }
                        }
                        proto
                            .send_msg(ConfigMsgOut::AppState(res_layout_id, &values))
                            .await
                    } else {
                        defmt::warn!("SetAppParams layout_id={} timed out", layout_id);
                        proto.send_msg(ConfigMsgOut::AppState(layout_id, &[])).await
                    }
                }
            }
            ConfigMsgIn::GetAllAppParams => {
                let layout_ids = layout.get_layout_ids();
                let app_count = layout_ids.len();

                let mut res = proto.send_msg(ConfigMsgOut::BatchMsgStart(app_count)).await;

                if app_count > 0 && res.is_ok() {
                    if layout_spawn_active() || spawn_start_held() {
                        // Spawn in progress — answer immediately so health-check
                        // / recall paths are not starved for seconds.
                        for id in layout_ids {
                            if res.is_err() {
                                break;
                            }
                            res = proto.send_msg(ConfigMsgOut::AppState(id, &[])).await;
                        }
                    } else {
                        for id in layout_ids {
                            APP_PARAM_SIGNALS[id as usize].signal(AppParamCmd::RequestParamValues);
                        }
                        let receiver = async {
                            for _ in 0..app_count {
                                let (res_layout_id, values) = APP_PARAM_CHANNEL.receive().await;
                                proto
                                    .send_msg(ConfigMsgOut::AppState(res_layout_id, &values))
                                    .await?;
                            }
                            Ok(())
                        };

                        let batch_timeout_ms =
                            APP_PARAM_GET_TIMEOUT_MS.saturating_mul(app_count.max(1) as u64);
                        if let Ok(receiver_res) =
                            with_timeout(Duration::from_millis(batch_timeout_ms), receiver).await
                        {
                            res = receiver_res;
                        }
                    }
                }

                if res.is_ok() {
                    res = proto.send_msg(ConfigMsgOut::BatchMsgEnd).await;
                }
                res
            }
            ConfigMsgIn::SetGlobalConfig(mut global_config) => {
                global_config.validate();
                let sender = GLOBAL_CONFIG_WATCH.sender();
                sender.send(global_config);
                Ok(())
            }
            ConfigMsgIn::SetLayout(mut new_layout) => {
                new_layout.validate(get_channels);
                let n = new_layout.get_layout_ids().len();
                defmt::info!(
                    "cfg SetLayout apps={} (ACK then watch; dbg phase before={})",
                    n,
                    LAYOUT_DEBUG_PHASE.load(Ordering::Relaxed)
                );
                let sender = LAYOUT_WATCH.sender();
                // ACK first so the host sees Layout before the spawn storm.
                let res = proto
                    .send_msg(ConfigMsgOut::Layout(new_layout.clone()))
                    .await;
                layout = new_layout.clone();
                // Brief yield so USB TX can finish before Core 1 floods MIDI.
                Timer::after_millis(50).await;
                sender.send(new_layout);
                defmt::info!("cfg SetLayout watch sent");
                res
            }
            ConfigMsgIn::FactoryReset => {
                factory_reset().await;
                Ok(())
            }
            ConfigMsgIn::HoldPerfMute => {
                crate::tasks::midi::set_host_holds_perf_mute(true);
                defmt::info!("cfg HoldPerfMute");
                proto.send_msg(ConfigMsgOut::Pong).await
            }
            ConfigMsgIn::ReleasePerfMute => {
                // Persist + unmute on Core1. Do NOT kick a deferred multi-app
                // spawn — that burst wedged USB. Apps are already spawned by
                // per-step SetLayout under Hold.
                defmt::info!("cfg ReleasePerfMute → store + unmute (async)");
                RELEASE_SPAWN_DONE.reset();
                RELEASE_SPAWN_SIGNAL.signal(());
                proto.send_msg(ConfigMsgOut::Pong).await
            }
        };
        if let Err(err) = res {
            defmt::warn!("Failed to send config response: {}", err);
        }
        // Keep hold briefly so the SysEx TX finishes before Local CCs resume.
        Timer::after_millis(8).await;
        set_config_holds_perf_usb(false);
    }
}

/// Config protocol transport: reads reassembled SysEx frame bodies from
/// CONFIG_RX_CHANNEL and writes responses as cable-1 SysEx over the shared
/// USB-MIDI sender. Wire format: see libfp::sysex.
struct ConfigTransport<'a> {
    usb_tx: &'a SharedUsbSender<'a>,
    plain_buf: [u8; MAX_PLAIN_SIZE],
    frame_buf: [u8; MAX_SYSEX_FRAME],
}

impl<'a> ConfigTransport<'a> {
    fn new(usb_tx: &'a SharedUsbSender<'a>) -> Self {
        ConfigTransport {
            usb_tx,
            plain_buf: [0; MAX_PLAIN_SIZE],
            frame_buf: [0; MAX_SYSEX_FRAME],
        }
    }

    async fn read_msg(&mut self) -> Result<ConfigMsgIn, ProtocolError> {
        let frame = CONFIG_RX_CHANNEL.receive().await;
        let packed = frame
            .strip_prefix(&SYSEX_HEADER[..])
            .ok_or(ProtocolError::CorruptedMessage)?;
        let plain_len =
            unpack_7bit(packed, &mut self.plain_buf).map_err(|_| ProtocolError::DecodingError)?;
        if plain_len < 2 {
            return Err(ProtocolError::CorruptedMessage);
        }
        let payload_len = ((self.plain_buf[0] as usize) << 8) | self.plain_buf[1] as usize;
        if payload_len != plain_len - 2 {
            return Err(ProtocolError::CorruptedMessage);
        }
        from_bytes(&self.plain_buf[2..plain_len]).map_err(|_| ProtocolError::DecodingError)
    }

    async fn send_msg(&mut self, msg: ConfigMsgOut<'_>) -> Result<(), ProtocolError> {
        let payload_len = to_slice(&msg, &mut self.plain_buf[2..])
            .map_err(|_| ProtocolError::EncodingError)?
            .len();
        self.plain_buf[0] = ((payload_len >> 8) & 0xFF) as u8;
        self.plain_buf[1] = (payload_len & 0xFF) as u8;
        let plain_len = payload_len + 2;

        self.frame_buf[0] = SYSEX_START;
        self.frame_buf[1..1 + SYSEX_HEADER.len()].copy_from_slice(&SYSEX_HEADER);
        let packed_len = pack_7bit(
            &self.plain_buf[..plain_len],
            &mut self.frame_buf[1 + SYSEX_HEADER.len()..MAX_SYSEX_FRAME - 1],
        )
        .map_err(|_| ProtocolError::BufferTooSmall)?;
        let frame_len = 1 + SYSEX_HEADER.len() + packed_len + 1;
        self.frame_buf[frame_len - 1] = SYSEX_EOX;

        // Packetize into USB-MIDI event packets on the performance cable
        // (cable 0), flushed per 64-byte USB packet. macOS/CoreMIDI often
        // exposes only one port mapped to cable 0; sending on both cables
        // interleaves/corrupts SysEx on that single port. Cable 1 is still
        // accepted on RX for multi-cable hosts. The sender mutex is released
        // between USB packets so other MIDI can interleave during long
        // transfers.
        let mut usb_packet = [0u8; USB_MAX_PACKET_SIZE as usize];
        let mut usb_len = 0;
        let total_chunks = frame_len.div_ceil(3);
        let mut last_write_len = 0;
        for (i, chunk) in self.frame_buf[..frame_len].chunks(3).enumerate() {
            let last = i + 1 == total_chunks;
            let cin: u8 = if last {
                // SysEx ends with following 1/2/3 bytes
                match chunk.len() {
                    1 => 0x5,
                    2 => 0x6,
                    _ => 0x7,
                }
            } else {
                // SysEx starts or continues
                0x4
            };
            usb_packet[usb_len] = (PERF_CABLE << 4) | cin;
            usb_packet[usb_len + 1..usb_len + 4].fill(0);
            usb_packet[usb_len + 1..usb_len + 1 + chunk.len()].copy_from_slice(chunk);
            usb_len += 4;
            if usb_len == usb_packet.len() || last {
                write_usb_packet(self.usb_tx, &usb_packet[..usb_len]).await?;
                last_write_len = usb_len;
                usb_len = 0;
            }
        }
        if last_write_len == usb_packet.len() {
            // Terminate the bulk transfer with a ZLP after a full-size packet
            write_usb_packet(self.usb_tx, &[]).await?;
        }

        Ok(())
    }
}

async fn write_usb_packet(usb_tx: &SharedUsbSender<'_>, data: &[u8]) -> Result<(), ProtocolError> {
    // Lock inside the timeout so a wedged USB write cannot pin SharedUsbSender
    // and starve midi_out / the next config reply.
    with_timeout(Duration::from_millis(CONFIG_WRITE_TIMEOUT_MS), async {
        let mut tx = usb_tx.lock().await;
        tx.write_packet(data).await
    })
    .await
    .map_err(|_| ProtocolError::Timeout)?
    .map_err(|_| ProtocolError::TransmissionError)
}
