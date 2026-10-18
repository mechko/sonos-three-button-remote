#![no_std]
#![no_main]

#[path = "../src/network.rs"]
#[allow(dead_code)]
mod network;
#[path = "../src/sonos.rs"]
#[allow(dead_code)]
mod sonos;

#[path = "../src/wifi.rs"]
mod wifi;

use core::{fmt::Write, panic::PanicInfo};
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    uart::{Config as UartConfig, Uart},
};
use esp_radio::wifi::{
    sta::StationConfig, AuthenticationMethodConfig, Config as WifiConfig, WifiController,
};
use smoltcp::{
    iface::SocketStorage,
    socket::{tcp, udp},
};
use sonos::TextBuffer;
use wifi::current_millis;

include!(concat!(env!("OUT_DIR"), "/ssid.rs"));
include!(concat!(env!("OUT_DIR"), "/password.rs"));

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 100 * 1024);
    let timers = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timers.timer0, peripherals.FROM_CPU_INTR0);
    let delay = Delay::new();
    let mut serial = Uart::new(peripherals.UART0, UartConfig::default())
        .unwrap()
        .with_tx(peripherals.GPIO21)
        .with_rx(peripherals.GPIO20);
    let _ = writeln!(
        serial,
        "Target: {} ({})\r",
        sonos::TARGET_NAME,
        sonos::TARGET_MODEL
    );
    let _ = writeln!(serial, "\r\nESP32-C3 Sonos discovery; no display\r");
    delay.delay_millis(500u32);
    macro_rules! status {
        ($text:expr) => {{
            let _ = writeln!(serial, "{}\r", $text);
        }};
    }
    macro_rules! fatal {
        ($text:expr) => {{
            status!($text);
            loop {
                delay.delay_millis(1000u32);
            }
        }};
    }
    status!("Target speaker\nStarting Wi-Fi...");
    let authentication = if PASSWORD.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(
            PASSWORD.try_into().expect("Invalid password length"),
        )
    };
    let config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(SSID.try_into().expect("Invalid SSID length"))
            .with_authentication(authentication)
            // Prefer B/G for this low-bandwidth remote: N mode showed delayed
            // DHCP and packet loss with the tested access point.
            .with_protocols(
                esp_radio::wifi::Protocols::default()
                    .with_2_4(esp_radio::wifi::Protocol::B | esp_radio::wifi::Protocol::G),
            ),
    );
    let mut controller = match WifiController::new(
        peripherals.WIFI,
        esp_radio::wifi::ControllerConfig::default().with_initial_config(config),
    ) {
        Ok(controller) => controller,
        Err(_) => fatal!("Wi-Fi init failed\nPress EN to retry"),
    };
    let radio = esp_radio::wifi::Interface::station();
    let mac = smoltcp::wire::EthernetAddress(radio.mac_address());
    let mut device = wifi::DeviceAdapter(radio);
    let mut iface_config = smoltcp::iface::Config::new(mac.into());
    let rng = esp_hal::rng::Rng::new();
    iface_config.random_seed = ((rng.random() as u64) << 32) | rng.random() as u64;
    let iface = smoltcp::iface::Interface::new(
        iface_config,
        &mut device,
        smoltcp::time::Instant::from_millis(current_millis() as i64),
    );
    let mut socket_storage: [SocketStorage; 3] = Default::default();
    let mut sockets = smoltcp::iface::SocketSet::new(&mut socket_storage[..]);
    sockets.add(smoltcp::socket::dhcpv4::Socket::new());
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
    let mut response = [0; 16384];
    let mut target = None;
    loop {
        if !controller.is_connected() {
            target = None;
            network.reset_dhcp();
            status!("Target speaker\nConnecting Wi-Fi...");
            // Bound the async connection attempt so failed networks can be retried.
            if !matches!(
                wifi::wait_with_tick(controller.connect_async(), 30000, || {}),
                Some(Ok(_))
            ) {
                status!("Wi-Fi connection failed or timed out");
            }
            if !controller.is_connected() {
                status!("Wi-Fi failed\nRetrying...");
                let _ = wifi::wait_with_tick(controller.disconnect_async(), 5000, || {});
                network.wait(3000);
                continue;
            }
        }
        if network.ip().is_none() {
            let _ = writeln!(serial, "Wi-Fi signal: {:?} dBm\r", controller.rssi());
            status!("Wi-Fi connected\nGetting IP...");
            let deadline = current_millis() + 20000;
            while current_millis() < deadline && network.ip().is_none() && controller.is_connected()
            {
                network.poll();
            }
            if network.ip().is_none() {
                // Let smoltcp finish its DHCP retry/backoff cycle. Reassociating
                // here aborts pending offers/requests on slow or lossy networks.
                status!("No network address yet\nContinuing DHCP...");
                continue;
            }
            let _ = writeln!(serial, "Network address: {}\r", network.ip().unwrap());
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
