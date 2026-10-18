#![no_std]
#![no_main]

use core::{fmt::Write, panic::PanicInfo};
#[path = "../src/button.rs"]
mod button;
#[path = "../src/sonos.rs"]
#[allow(dead_code)]
mod sonos;
use esp32c3_hal::{
    clock::ClockControl, entry, peripherals::Peripherals, prelude::*, timer::TimerGroup, Delay,
    Rtc, Uart, IO,
};

#[entry]
fn main() -> ! {
    let peripherals = Peripherals::take();
    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::boot_defaults(system.clock_control).freeze();
    let mut rtc = Rtc::new(peripherals.RTC_CNTL);
    rtc.rwdt.disable();
    let mut timers0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    let mut timers1 = TimerGroup::new(peripherals.TIMG1, &clocks);
    timers0.wdt.disable();
    timers1.wdt.disable();

    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    // Each switch connects its GPIO to GND; internal pull-ups hold released inputs high.
    let dlf = io.pins.gpio0.into_pull_up_input();
    let radioeins = io.pins.gpio1.into_pull_up_input();
    let kultur = io.pins.gpio10.into_pull_up_input();
    macro_rules! readings {
        () => {
            [
                dlf.is_low().unwrap(),
                radioeins.is_low().unwrap(),
                kultur.is_low().unwrap(),
            ]
        };
    }
    let mut delay = Delay::new(&clocks);
    let mut serial = Uart::new(peripherals.UART0, &clocks);
    let _ = writeln!(
        serial,
        "\r\nButton test: button 1=GPIO0, button 2=GPIO1, button 3=GPIO10; bridge to GND\r"
    );
    let mut buttons = button::Buttons::new(readings!(), 0);
    let mut now = 0;
    loop {
        delay.delay_ms(5u32);
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
