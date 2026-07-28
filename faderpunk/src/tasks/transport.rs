use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_rp::peripherals::USB;
use embassy_rp::usb;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_usb::class::midi::MidiClass;
use embassy_usb::{Builder, Config as UsbConfig, UsbDevice};
use static_cell::StaticCell;

use embassy_rp::uart::{Async, BufferedUart, UartTx};

use crate::tasks::configure::start_config_loop;
use crate::tasks::midi::{midi_in_task, midi_out_task};
use crate::version::USB_RELEASE_VERSION;

const USB_VENDOR_ID: u16 = 0xf569;
const USB_PRODUCT_ID: u16 = 0x1;
const USB_VENDOR_NAME: &str = "ATOV";
const USB_PRODUCT_NAME: &str = "Faderpunk";

pub const USB_MAX_PACKET_SIZE: u16 = 64;

static USB_CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
static USB_BOS_DESC: StaticCell<[u8; 128]> = StaticCell::new();
static USB_CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
static USB_SERIAL_BUF: StaticCell<[u8; 16]> = StaticCell::new();
static USB_TX: StaticCell<
    Mutex<NoopRawMutex, embassy_usb::class::midi::Sender<'static, usb::Driver<'static, USB>>>,
> = StaticCell::new();

pub async fn start_transports(
    spawner: &Spawner,
    usb_driver: usb::Driver<'static, USB>,
    uart0: UartTx<'static, Async>,
    uart1: BufferedUart,
    chip_id: u64,
) {
    spawner
        .spawn(run_transports(usb_driver, uart0, uart1, chip_id))
        .unwrap();
}

/// Poll the USB device independently of MIDI/config work. When `usb.run()`
/// shared a `join4` with midi_out, a SetLayout mute-drain (Local events that
/// complete without await) starved SOFs and macOS dropped the MIDI device.
#[embassy_executor::task]
async fn usb_device_task(mut usb: UsbDevice<'static, usb::Driver<'static, USB>>) {
    usb.run().await;
}

#[embassy_executor::task]
#[allow(clippy::large_futures)]
async fn run_transports(
    usb_driver: usb::Driver<'static, USB>,
    uart0_tx: UartTx<'static, Async>,
    uart1: BufferedUart,
    chip_id: u64,
) {
    let serial_buf = USB_SERIAL_BUF.init([0u8; 16]);
    let chip_id_bytes = chip_id.to_be_bytes();
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for (i, &byte) in chip_id_bytes.iter().enumerate() {
        serial_buf[i * 2] = HEX[(byte >> 4) as usize];
        serial_buf[i * 2 + 1] = HEX[(byte & 0x0F) as usize];
    }
    // Safety: We just filled the buffer with valid ASCII hex chars
    let serial_number = unsafe { core::str::from_utf8_unchecked(serial_buf) };

    let mut usb_config = UsbConfig::new(USB_VENDOR_ID, USB_PRODUCT_ID);
    usb_config.manufacturer = Some(USB_VENDOR_NAME);
    usb_config.product = Some(USB_PRODUCT_NAME);
    usb_config.serial_number = Some(serial_number);
    usb_config.device_release = USB_RELEASE_VERSION;
    usb_config.max_power = 500;
    usb_config.max_packet_size_0 = USB_MAX_PACKET_SIZE as u8;
    // Deliberately a pure single-function MIDI-class device: bDeviceClass 0x00,
    // no IADs, no vendor interfaces, no MS OS descriptors. Embedded USB MIDI
    // hosts refuse to exchange MIDI with devices carrying any non-MIDI
    // interface (see docs/usb-host-compatibility.md), which is why the
    // configurator protocol runs as SysEx on the second virtual MIDI cable.

    let config_descriptor = USB_CONFIG_DESC.init([0; 256]);
    let bos_descriptor = USB_BOS_DESC.init([0; 128]);
    let control_buf = USB_CONTROL_BUF.init([0; 64]);

    let mut usb_builder = Builder::new(
        usb_driver,
        usb_config,
        config_descriptor,
        bos_descriptor,
        &mut [], // no MS OS descriptors
        control_buf,
    );

    // Two virtual cables: cable 0 = performance MIDI, cable 1 = config SysEx
    let usb_midi = MidiClass::new(&mut usb_builder, 2, 2, USB_MAX_PACKET_SIZE);

    let (usb_tx_raw, usb_rx) = usb_midi.split();
    // Shared between performance MIDI out and the config loop. Both still run
    // in this task (join3), so NoopRawMutex remains valid. Must be 'static
    // because UsbDevice is spawned on a separate task with 'static lifetime.
    let usb_tx = USB_TX.init(Mutex::<NoopRawMutex, _>::new(usb_tx_raw));
    let (uart1_tx, uart1_rx) = uart1.split();

    let usb = usb_builder.build();
    Spawner::for_current_executor()
        .await
        .spawn(usb_device_task(usb))
        .unwrap();

    let midi_out_fut = midi_out_task(usb_tx, uart0_tx, uart1_tx);
    let midi_in_fut = midi_in_task(usb_rx, uart1_rx);
    let config_fut = start_config_loop(usb_tx);

    join3(midi_in_fut, midi_out_fut, config_fut).await;
}
