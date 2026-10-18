#![no_std]
#![no_main]

mod button;
mod led;
mod network;
mod sonos;
mod wifi;

use core::{cell::Cell, fmt::Write, panic::PanicInfo};
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Pull},
    uart::{Config as UartConfig, Uart},
};
use esp_hal::{
    gpio::Level,
    rmt::{PulseCode, Rmt, TxChannelConfig, TxChannelCreator},
    time::Rate,
};
use esp_radio::wifi::{
    sta::StationConfig, AuthenticationMethodConfig, Config as WifiConfig, WifiController,
};
use led::Status as LedStatus;
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
    // Active-low buttons connect to GND; pull-ups hold released inputs high.
    let button_one = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );
    let button_two = Input::new(
        peripherals.GPIO1,
        InputConfig::default().with_pull(Pull::Up),
    );
    let button_three = Input::new(
        peripherals.GPIO10,
        InputConfig::default().with_pull(Pull::Up),
    );
    macro_rules! readings {
        () => {
            [
                button_one.is_low(),
                button_two.is_low(),
                button_three.is_low(),
            ]
        };
    }
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
    let _ = writeln!(
        serial,
        "\r\nSonos buttons: 1=GPIO0, 2=GPIO1, 3=GPIO10; press to GND\r"
    );
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let channel = rmt
        .channel0
        .configure_tx(
            &TxChannelConfig::default()
                .with_clk_divider(1)
                .with_idle_output(true)
                .with_idle_output_level(Level::Low),
        )
        .unwrap()
        .with_pin(peripherals.GPIO8);
    let mut led_channel = Some(channel);
    let led_status = Cell::new(LedStatus::WifiDown);
    let mut last_led_status = None;
    let mut last_led_color = None;
    let mut animation_start = 0;
    let mut last_led_update = 0;
    // RMT generates the waveform independently of Wi-Fi interrupts.
    // Only transmit on changes; retain the channel for retries after errors.
    let mut set_led = |state: LedStatus| {
        let now = current_millis();
        led_status.set(state);
        let changed = last_led_status != Some(state);
        if changed {
            animation_start = now;
        }
        if !changed && now.saturating_sub(last_led_update) < 20 {
            return;
        }
        let color = led::color(state, now.saturating_sub(animation_start));
        if !changed && last_led_color == Some(color) {
            return;
        }
        let words = led::frame(color).map(PulseCode::from);
        let result = led_channel
            .take()
            .unwrap()
            .transmit(&words)
            .and_then(|transaction| transaction.wait());
        match result {
            Ok(channel) => {
                led_channel = Some(channel);
                last_led_status = Some(state);
                last_led_color = Some(color);
                last_led_update = now;
            }
            Err((_, channel)) => {
                led_channel = Some(channel);
            }
        }
    };
    delay.delay_micros(300u32);
    set_led(LedStatus::WifiDown);

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
    let mut button_state = button::Buttons::new(readings!(), current_millis());
    let mut next_check = 0;
    let mut next_playback_check = 0;
    macro_rules! sample {
        () => {
            button_state.update(readings!(), current_millis())
        };
    }
    macro_rules! tick {
        () => {
            || {
                sample!();
                if !controller.is_connected() {
                    set_led(LedStatus::WifiDown);
                } else {
                    set_led(led_status.get());
                }
            }
        };
    }
    loop {
        sample!();
        if !controller.is_connected() {
            set_led(LedStatus::WifiDown);
            target = None;
            network.reset_dhcp();
            status!("Target speaker\nConnecting Wi-Fi...");
            // Bound the async connection attempt while continuing input sampling.
            if !matches!(
                wifi::wait_with_tick(controller.connect_async(), 30000, || {
                    sample!();
                    set_led(LedStatus::WifiDown);
                }),
                Some(Ok(_))
            ) {
                status!("Wi-Fi connection failed or timed out");
            }
            if !controller.is_connected() {
                status!("Wi-Fi failed\nRetrying...");
                let _ = wifi::wait_with_tick(controller.disconnect_async(), 5000, || {
                    sample!();
                    set_led(LedStatus::WifiDown);
                });
                network.wait_with_tick(3000, tick!());
                continue;
            }
        }
        if network.ip().is_none() {
            target = None;
            set_led(LedStatus::WifiDown);
            let _ = writeln!(serial, "Wi-Fi signal: {:?} dBm\r", controller.rssi());
            status!("Wi-Fi connected\nGetting IP...");
            let deadline = current_millis() + 20000;
            while current_millis() < deadline && network.ip().is_none() && controller.is_connected()
            {
                network.poll();
                sample!();
            }
            if network.ip().is_none() {
                // Let smoltcp finish its DHCP retry/backoff cycle. Reassociating
                // here aborts pending offers/requests on slow or lossy networks.
                status!("No network address yet\nContinuing DHCP...");
                continue;
            }
            let _ = writeln!(serial, "Network address: {}\r", network.ip().unwrap());
        }
        if target.is_none() {
            set_led(LedStatus::SonosUnavailable);
            status!("Searching Sonos...\nTarget speaker");
            let (candidates, count) = network.discover_with_tick(tick!());
            for candidate in candidates.into_iter().flatten() {
                match network.fetch_with_tick(candidate, &mut response, tick!()) {
                    Ok(xml) if sonos::is_target(xml) => {
                        target = Some(candidate);
                        let _ = writeln!(serial, "Target speaker verified at {}\r", candidate.ip);
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let _ = writeln!(serial, "{}: {error}\r", candidate.ip);
                    }
                }
            }
            if target.is_none() {
                let mut text = TextBuffer::<128>::new();
                let _ = write!(
                    text,
                    "Sonos not found\nTarget speaker\n{count} replies\nRetrying..."
                );
                status!(text.as_str());
                network.wait_with_tick(10000, tick!());
                continue;
            }
            set_led(LedStatus::Ready);
            status!("Ready\nTarget speaker\nPress for radio");
            next_check = current_millis() + 30000;
            next_playback_check = 0;
        }
        let candidate = target.unwrap();
        let wants_radio = button_state.has_press();
        if wants_radio || current_millis() >= next_check {
            // Verify the name/model before every playback command, so an IP
            // reassigned by DHCP cannot cause playback on a different speaker.
            match network.fetch_with_tick(candidate, &mut response, tick!()) {
                Ok(xml) if sonos::is_target(xml) => {}
                _ => {
                    set_led(if controller.is_connected() {
                        LedStatus::SonosUnavailable
                    } else {
                        LedStatus::WifiDown
                    });
                    target = None;
                    status!("Sonos unavailable\nSearching again...");
                    continue;
                }
            }
            next_check = current_millis() + 30000;
        }
        if let Some(index) = button_state.take_press() {
            let station = sonos::Station::ALL[index];
            status!("Starting radio...");
            let _ = writeln!(serial, "{}: {}\r", station.title(), station.url());
            // One initial attempt followed by at most three retries.
            let mut retries = 0;
            let result = loop {
                match play_radio(&mut network, candidate, station, &mut response, tick!()) {
                    Ok(()) => break Ok(()),
                    Err(error) => {
                        if retries == 3 {
                            break Err(error);
                        }
                        let _ = writeln!(serial, "Playback attempt failed: {error}\r");
                        network.wait_with_tick(1000, tick!());
                        // Recheck identity before sending another playback command.
                        if !controller.is_connected()
                            || network.ip().is_none()
                            || !matches!(
                                network.fetch_with_tick(candidate, &mut response, tick!()),
                                Ok(xml) if sonos::is_target(xml)
                            )
                        {
                            break Err(network::ControlError::Network("Sonos unavailable"));
                        }
                        retries += 1;
                        let _ = writeln!(serial, "Retrying radio ({retries}/3)...\r");
                    }
                }
            };
            match result {
                Ok(()) => {
                    set_led(LedStatus::Playing);
                    next_playback_check = current_millis() + 3000;
                    let _ = writeln!(serial, "Playing radio: {}\r", station.title());
                }
                Err(error) => {
                    // Playback rejection is distinct from loss of reachability.
                    let reachable = matches!(
                        network.fetch_with_tick(candidate, &mut response, tick!()),
                        Ok(xml) if sonos::is_target(xml)
                    );
                    set_led(if !controller.is_connected() || network.ip().is_none() {
                        target = None;
                        LedStatus::WifiDown
                    } else if reachable {
                        LedStatus::Ready
                    } else {
                        target = None;
                        LedStatus::SonosUnavailable
                    });
                    let mut text = TextBuffer::<160>::new();
                    let _ = write!(text, "Radio failed\n{error}\nPress to retry");
                    status!(text.as_str());
                }
            }
        }
        if target.is_some() && current_millis() >= next_playback_check {
            match radio_playing(&mut network, candidate, &mut response, tick!()) {
                Ok(true) => set_led(LedStatus::Playing),
                Ok(false) => set_led(LedStatus::Ready),
                Err(error) => {
                    let _ = writeln!(serial, "Playback check: {error}\r");
                    // An unsupported query is not proof of a lost connection.
                    let reachable = matches!(
                        network.fetch_with_tick(candidate, &mut response, tick!()),
                        Ok(xml) if sonos::is_target(xml)
                    );
                    set_led(if !controller.is_connected() || network.ip().is_none() {
                        target = None;
                        LedStatus::WifiDown
                    } else if reachable {
                        LedStatus::Ready
                    } else {
                        target = None;
                        LedStatus::SonosUnavailable
                    });
                }
            }
            next_playback_check = current_millis() + 3000;
        }
        set_led(led_status.get());
        network.poll();
        delay.delay_millis(5u32);
    }
}

fn radio_playing<D: smoltcp::phy::Device>(
    network: &mut network::Network<'_, D>,
    target: network::Candidate,
    response: &mut [u8],
    mut tick: impl FnMut(),
) -> Result<bool, network::ControlError> {
    let xml =
        network.soap_with_tick(target, sonos::Action::GetTransportInfo, response, &mut tick)?;
    if sonos::xml_value(xml, "CurrentTransportState") != Some("PLAYING") {
        return Ok(false);
    }
    let xml = network.soap_with_tick(target, sonos::Action::GetMediaInfo, response, &mut tick)?;
    Ok(sonos::is_radio_media(xml))
}

fn play_radio<D: smoltcp::phy::Device>(
    network: &mut network::Network<'_, D>,
    target: network::Candidate,
    station: sonos::Station,
    response: &mut [u8],
    mut tick: impl FnMut(),
) -> Result<(), network::ControlError> {
    network.soap_with_tick(
        target,
        sonos::Action::SetRadio(station),
        response,
        &mut tick,
    )?;
    network.soap_with_tick(target, sonos::Action::Play, response, &mut tick)?;
    let deadline = current_millis() + 8000;
    loop {
        let xml =
            network.soap_with_tick(target, sonos::Action::GetTransportInfo, response, &mut tick)?;
        if sonos::xml_value(xml, "CurrentTransportState") == Some("PLAYING") {
            return Ok(());
        }
        if current_millis() >= deadline {
            return Err(network::ControlError::Network("Not playing yet"));
        }
        network.wait_with_tick(500, &mut tick);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
