# OLED display reference

The display is optional for future projects. The user no longer needs it for
the Sonos button project. This document preserves the information needed to
reuse it with other programs; the current C3 firmware and examples have had their display code removed.

## Hardware and interface

The previous firmware treated the display as an **SSD1306-compatible, 128×64
monochrome OLED with an I²C interface**. This configuration comes from the
working project code; the exact display manufacturer and part number have not
been recorded.

- Power: connect VCC to **3V3** and GND to **GND**.
- I²C address: try **0x3C**, then **0x3D** (7-bit addresses).
- I²C speed used by this project: **100 kHz**.
- Orientation used by this project: `DisplayRotation::Rotate0`.

## Wiring

| OLED connection | Original ESP32-WROOM-32 wiring | Proposed ESP32-C3-DevKitM-1 wiring |
| --- | --- | --- |
| VCC | 3V3 | 3V3 |
| GND | GND | GND |
| SDA | GPIO17 | GPIO4 |
| SCL | GPIO16 | GPIO5 |

The C3 wiring was selected from the pinout supplied by the user. It has not
been tested on hardware. GPIO4 and GPIO5 are available for I²C when they are
not being used for external JTAG. These are project pin choices, not fixed
I²C pins required by the display.

## Existing Rust implementation

The previous ESP32 firmware used a multiline status renderer, a centered
`pressed` / `not pressed` test, and Sonos discovery status text. Those renderers
have now been removed so the C3 can operate with no display attached.

The previous implementation used these display-related dependencies (now removed):

```toml
embedded-graphics = "=0.8.1"
ssd1306 = "=0.8.4"
display-interface = "=0.4.1"
```

Initialization follows this sequence:

1. Create the board's I²C peripheral with the selected SDA/SCL pins at 100 kHz.
2. Probe addresses `0x3C` and `0x3D` using `i2c.write(address, &[0x00, 0xAE])`.
   The previous firmware retried until one responded. `0xAE` turns the display
   off; the subsequent initialization configures it for use.
3. Wrap the I²C peripheral with
   `I2CDisplayInterface::new_custom_address(i2c, address)`.
4. Construct `Ssd1306::new(interface, DisplaySize128x64,
   DisplayRotation::Rotate0).into_buffered_graphics_mode()`.
5. Call `display.init()`.
6. For each update, call `clear_buffer()`, draw text or graphics, and call
   `flush()` to send the framebuffer to the screen.

A 128×64 monochrome framebuffer requires 1,024 bytes. Rendering occurs in RAM;
changes become visible after `flush()`.

The previous main firmware used `embedded_graphics::mono_font::iso_8859_1::FONT_6X10`
with `BinaryColor::On`, top-aligned text starting at `(0, 0)`, and 11-pixel
line spacing. Its helper drew at most six lines. This font includes the
umlaut used in “Küche”. The previous button example used `FONT_10X20` with centered
text at `(64, 32)`.

## Reuse notes for future agents

The drawing approach is reusable, but board initialization is chip-specific.
The previous code used `esp32_hal`; the current project uses `esp32c3_hal` and
builds for RISC-V. Use the C3 wiring above when restoring display support.

The previous firmware waited indefinitely for a display to respond, ignored
rendering/flush errors, and unwrapped initialization. Prefer explicit error
handling and allow headless startup when adding display support to a program.
The current firmware starts without a display.

Display operation has not been independently verified by an agent on hardware.
