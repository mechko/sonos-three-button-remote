#![no_std]
#![no_main]

#[path = "../src/network.rs"]
#[allow(dead_code)]
mod network;
#[path = "../src/sonos.rs"]
#[allow(dead_code)]
mod sonos;

use core::{fmt::Write, panic::PanicInfo};
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};
use esp32c3_hal::{
    clock::ClockControl, entry, peripherals::Peripherals, prelude::*, timer::TimerGroup, Delay,
    Rng, Rtc, Uart, IO,
};
use esp_wifi::{
    current_millis, initialize,
    wifi::{utils::create_network_interface, WifiStaDevice},
    EspWifiInitFor,
};
use smoltcp::{
    iface::SocketStorage,
    socket::{tcp, udp},
};
use sonos::TextBuffer;

include!(concat!(env!("OUT_DIR"), "/ssid.rs"));
include!(concat!(env!("OUT_DIR"), "/password.rs"));

#[entry]
fn main() -> ! {
    let peripherals = Peripherals::take();
    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::max(system.clock_control).freeze();
    let mut rtc = Rtc::new(peripherals.RTC_CNTL);
    rtc.rwdt.disable();
    let mut timers0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    let mut timers1 = TimerGroup::new(peripherals.TIMG1, &clocks);
    timers0.wdt.disable();
    timers1.wdt.disable();
    let _io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    let mut delay = Delay::new(&clocks);
    let mut serial = Uart::new(peripherals.UART0, &clocks);
    let _ = writeln!(
        serial,
        "Target: {} ({})\r",
        sonos::TARGET_NAME,
        sonos::TARGET_MODEL
    );
    let _ = writeln!(serial, "\r\nESP32-C3 Sonos discovery; no display\r");
    delay.delay_ms(500u32);
    macro_rules! status {
        ($text:expr) => {{
            let _ = writeln!(serial, "{}\r", $text);
        }};
    }
    macro_rules! fatal {
        ($text:expr) => {{
            status!($text);
            loop {
                delay.delay_ms(1000u32);
            }
        }};
    }
    status!("Target speaker\nStarting Wi-Fi...");
    let init = match initialize(
        EspWifiInitFor::Wifi,
        esp32c3_hal::systimer::SystemTimer::new(peripherals.SYSTIMER).alarm0,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    ) {
        Ok(init) => init,
        Err(error) => {
            let _ = writeln!(serial, "Wi-Fi init: {error:?}\r");
            fatal!("Wi-Fi init failed\nPress EN to retry");
        }
    };
    let mut socket_storage: [SocketStorage; 3] = Default::default();
    let (iface, device, mut controller, mut sockets) =
        match create_network_interface(&init, peripherals.WIFI, WifiStaDevice, &mut socket_storage)
        {
            Ok(network) => network,
            Err(error) => {
                let _ = writeln!(serial, "Network init: {error:?}\r");
                fatal!("Network init failed\nPress EN to retry");
            }
        };
    let mut udp_rx_metadata = [udp::PacketMetadata::EMPTY; 8];
    let mut udp_tx_metadata = [udp::PacketMetadata::EMPTY; 2];
    let mut udp_rx = [0; 8192];
    let mut udp_tx = [0; 512];
    let mut udp_socket = udp::Socket::new(
        udp::PacketBuffer::new(&mut udp_rx_metadata[..], &mut udp_rx[..]),
        udp::PacketBuffer::new(&mut udp_tx_metadata[..], &mut udp_tx[..]),
    );
    udp_socket.bind(49152).unwrap();
    let udp = sockets.add(udp_socket);
    let mut tcp_rx = [0; 2048];
    let mut tcp_tx = [0; 512];
    let tcp = sockets.add(tcp::Socket::new(
        tcp::SocketBuffer::new(&mut tcp_rx[..]),
        tcp::SocketBuffer::new(&mut tcp_tx[..]),
    ));
    let mut network = network::Network {
        iface,
        device,
        sockets,
        udp,
        tcp,
        next_port: 49153,
    };
    let config = Configuration::Client(ClientConfiguration {
        ssid: SSID.into(),
        password: PASSWORD.into(),
        ..Default::default()
    });
    if let Err(error) = controller
        .set_configuration(&config)
        .and_then(|_| controller.start())
    {
        let _ = writeln!(serial, "Wi-Fi setup: {error:?}\r");
        fatal!("Wi-Fi setup failed\nPress EN to retry");
    }
    let mut response = [0; 16384];
    let mut target = None;
    loop {
        if !matches!(controller.is_connected(), Ok(true)) {
            target = None;
            network.reset_dhcp();
            status!("Target speaker\nConnecting Wi-Fi...");
            if let Err(error) = controller.connect() {
                let _ = writeln!(serial, "Wi-Fi connect: {error:?}\r");
            }
            let deadline = current_millis() + 30000;
            while current_millis() < deadline && !matches!(controller.is_connected(), Ok(true)) {
                network.poll();
            }
            if !matches!(controller.is_connected(), Ok(true)) {
                status!("Wi-Fi failed\nRetrying...");
                let _ = controller.disconnect();
                network.wait(3000);
                continue;
            }
        }
        if network.ip().is_none() {
            status!("Wi-Fi connected\nGetting IP...");
            let deadline = current_millis() + 20000;
            while current_millis() < deadline && network.ip().is_none() {
                network.poll();
            }
            if network.ip().is_none() {
                status!("No network address\nRetrying Wi-Fi...");
                let _ = controller.disconnect();
                network.wait(1000);
                continue;
            }
        }
        if let Some(candidate) = target {
            match network.fetch(candidate, &mut response) {
                Ok(xml) if sonos::is_target(xml) => {
                    let mut text = TextBuffer::<128>::new();
                    let _ = write!(
                        text,
                        "Sonos connected\nTarget speaker\n{}\nWi-Fi OK",
                        candidate.ip
                    );
                    status!(text.as_str());
                    network.wait(10000);
                    continue;
                }
                Ok(_) => {
                    status!("Sonos identity changed\nSearching again...");
                }
                Err(error) => {
                    let _ = writeln!(serial, "Sonos unreachable: {error}\r");
                    status!("Sonos unreachable\nSearching again...");
                }
            }
            target = None;
        }
        let mut text = TextBuffer::<128>::new();
        let Some(local_ip) = network.ip() else {
            target = None;
            continue;
        };
        let _ = write!(
            text,
            "Wi-Fi connected\n{}\nSearching Sonos...\nTarget speaker",
            local_ip
        );
        status!(text.as_str());
        let (candidates, count) = network.discover();
        for candidate in candidates.into_iter().flatten() {
            let mut text = TextBuffer::<128>::new();
            let _ = write!(text, "Checking speaker\n{}\nTarget speaker", candidate.ip);
            status!(text.as_str());
            match network.fetch(candidate, &mut response) {
                Ok(xml) => {
                    let _ = writeln!(
                        serial,
                        "{}: room={:?}, model={:?}\r",
                        candidate.ip,
                        sonos::xml_value(xml, "roomName"),
                        sonos::xml_value(xml, "modelName")
                    );
                    if sonos::is_target(xml) {
                        target = Some(candidate);
                        break;
                    }
                }
                Err(error) => {
                    let _ = writeln!(serial, "{}: {error}\r", candidate.ip);
                }
            }
        }
        if target.is_none() {
            let mut text = TextBuffer::<128>::new();
            let _ = write!(
                text,
                "Sonos not found\nTarget speaker\n{count} replies\nRetry in 10 sec"
            );
            status!(text.as_str());
            network.wait(10000);
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
