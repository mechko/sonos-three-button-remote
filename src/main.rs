#![no_std]
#![no_main]

mod button;
mod led;
mod network;
mod sonos;

use core::{cell::Cell, fmt::Write, panic::PanicInfo};
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};
use esp32c3_hal::rmt::{Rmt, TxChannel, TxChannelConfig, TxChannelCreator};
use esp32c3_hal::{
    clock::ClockControl, entry, peripherals::Peripherals, prelude::*, timer::TimerGroup, Delay,
    Rng, Rtc, Uart, IO,
};
use esp_wifi::{
    current_millis, initialize,
    wifi::{utils::create_network_interface, WifiStaDevice},
    EspWifiInitFor,
};
use led::Status as LedStatus;
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
    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    // Active-low buttons connect to GND; internal pull-ups hold released inputs high.
    let button_one = io.pins.gpio0.into_pull_up_input();
    let button_two = io.pins.gpio1.into_pull_up_input();
    let button_three = io.pins.gpio10.into_pull_up_input();
    macro_rules! readings {
        () => {
            [
                button_one.is_low().unwrap(),
                button_two.is_low().unwrap(),
                button_three.is_low().unwrap(),
            ]
        };
    }
    let mut delay = Delay::new(&clocks);
    let mut serial = Uart::new(peripherals.UART0, &clocks);
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
    let rmt = Rmt::new(peripherals.RMT, 80u32.MHz(), &clocks).unwrap();
    let channel = rmt
        .channel0
        .configure(
            io.pins.gpio8,
            TxChannelConfig {
                clk_divider: 1,
                idle_output: true,
                idle_output_level: false,
                ..Default::default()
            },
        )
        .unwrap();
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
        let words = led::frame(color);
        let transaction = led_channel.take().unwrap().transmit(&words);
        match transaction.wait() {
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
    delay.delay_us(300u32);
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
                if !matches!(controller.is_connected(), Ok(true)) {
                    set_led(LedStatus::WifiDown);
                } else {
                    set_led(led_status.get());
                }
            }
        };
    }
    loop {
        sample!();
        if !matches!(controller.is_connected(), Ok(true)) {
            set_led(LedStatus::WifiDown);
            target = None;
            network.reset_dhcp();
            status!("Target speaker\nConnecting Wi-Fi...");
            if let Err(error) = controller.connect() {
                let _ = writeln!(serial, "Wi-Fi connect: {error:?}\r");
            }
            let deadline = current_millis() + 30000;
            while current_millis() < deadline && !matches!(controller.is_connected(), Ok(true)) {
                network.poll();
                sample!();
            }
            if !matches!(controller.is_connected(), Ok(true)) {
                status!("Wi-Fi failed\nRetrying...");
                let _ = controller.disconnect();
                network.wait_with_tick(3000, tick!());
                continue;
            }
        }
        if network.ip().is_none() {
            target = None;
            set_led(LedStatus::WifiDown);
            status!("Wi-Fi connected\nGetting IP...");
            let deadline = current_millis() + 20000;
            while current_millis() < deadline && network.ip().is_none() {
                network.poll();
                sample!();
            }
            if network.ip().is_none() {
                status!("No network address\nRetrying Wi-Fi...");
                let _ = controller.disconnect();
                network.wait_with_tick(1000, tick!());
                continue;
            }
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
                    set_led(if matches!(controller.is_connected(), Ok(true)) {
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
                        if !matches!(controller.is_connected(), Ok(true))
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
                    set_led(
                        if !matches!(controller.is_connected(), Ok(true)) || network.ip().is_none()
                        {
                            target = None;
                            LedStatus::WifiDown
                        } else if reachable {
                            LedStatus::Ready
                        } else {
                            target = None;
                            LedStatus::SonosUnavailable
                        },
                    );
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
                    set_led(
                        if !matches!(controller.is_connected(), Ok(true)) || network.ip().is_none()
                        {
                            target = None;
                            LedStatus::WifiDown
                        } else if reachable {
                            LedStatus::Ready
                        } else {
                            target = None;
                            LedStatus::SonosUnavailable
                        },
                    );
                }
            }
            next_playback_check = current_millis() + 3000;
        }
        set_led(led_status.get());
        network.poll();
        delay.delay_ms(5u32);
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
