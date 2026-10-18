#![no_std]
#![no_main]

use core::{fmt::Write, panic::PanicInfo};
#[path = "../src/button.rs"]
mod button;
#[path = "../src/sonos.rs"]
#[allow(dead_code)]
mod sonos;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Pull},
    uart::{Config as UartConfig, Uart},
};

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    // Active-low buttons connect to GND; pull-ups hold released inputs high.
    let dlf = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );
    let radioeins = Input::new(
        peripherals.GPIO1,
        InputConfig::default().with_pull(Pull::Up),
    );
    let kultur = Input::new(
        peripherals.GPIO10,
        InputConfig::default().with_pull(Pull::Up),
    );
    macro_rules! readings {
        () => {
            [dlf.is_low(), radioeins.is_low(), kultur.is_low()]
        };
    }
    let delay = Delay::new();
    let mut serial = Uart::new(peripherals.UART0, UartConfig::default())
        .unwrap()
        .with_tx(peripherals.GPIO21)
        .with_rx(peripherals.GPIO20);
    let _ = writeln!(
        serial,
        "\r\nButton test: button 1=GPIO0, button 2=GPIO1, button 3=GPIO10; bridge to GND\r"
    );
    let mut buttons = button::Buttons::new(readings!(), 0);
    let mut now = 0;
    loop {
        delay.delay_millis(5u32);
        now += 5;
        buttons.update(readings!(), now);
        if buttons.has_press() {
            while let Some(index) = buttons.take_press() {
                let name = sonos::STATIONS[index].title();
                let _ = writeln!(serial, "Button {}: {name}: pressed\r", index + 1);
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
