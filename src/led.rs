/// Status colors and animation for the onboard RGB LED.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    WifiDown,
    SonosUnavailable,
    Ready,
    Playing,
}

/// RMT words at 80 MHz: 0 = 0.4 us high / 0.85 us low,
/// 1 = 0.8 us high / 0.45 us low. Bytes are GRB, MSB first.
/// A 300 us low reset and an end marker finish each frame.
pub fn color(status: Status, elapsed_ms: u64) -> [u8; 3] {
    match status {
        Status::WifiDown => [0, 24, 0],
        Status::SonosUnavailable => [8, 24, 0],
        Status::Ready => [12, 0, 0],
        Status::Playing => {
            // Six-second breathing cycle with a squared brightness curve.
            let phase = (elapsed_ms % 6000) as u32;
            let ramp = if phase <= 3000 { phase } else { 6000 - phase };
            let brightness = 1 + 23 * ramp * ramp / 9_000_000;
            [brightness as u8, 0, 0]
        }
    }
}

pub fn frame(grb: [u8; 3]) -> [u32; 26] {
    let mut words = [0; 26];
    for (byte_index, byte) in grb.into_iter().enumerate() {
        for bit in 0..8 {
            let (high, low) = if byte & (0x80 >> bit) != 0 {
                (64, 36)
            } else {
                (32, 68)
            };
            words[byte_index * 8 + bit] = high | (1 << 15) | (low << 16);
        }
    }
    words[24] = 12000 | (12000 << 16);
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breathing_reaches_both_extremes_and_repeats_smoothly() {
        assert_eq!(color(Status::Playing, 0), [1, 0, 0]);
        assert_eq!(color(Status::Playing, 3000), [24, 0, 0]);
        assert_eq!(color(Status::Playing, 6000), [1, 0, 0]);
        for t in 0..3000 {
            assert!(color(Status::Playing, t)[0] <= color(Status::Playing, t + 1)[0]);
            assert_eq!(color(Status::Playing, t), color(Status::Playing, 6000 - t));
        }
    }

    #[test]
    fn colors_are_grb_msb_first_with_reset_and_end_marker() {
        for (status, expected) in [
            (Status::WifiDown, [0, 24, 0]),
            (Status::SonosUnavailable, [8, 24, 0]),
            (Status::Ready, [12, 0, 0]),
        ] {
            let words = frame(color(status, 0));
            let mut decoded = [0u8; 3];
            for (i, word) in words[..24].iter().copied().enumerate() {
                let high = word & 0x7fff;
                let low = (word >> 16) & 0x7fff;
                assert_eq!(high + low, 100);
                assert_eq!(word & (1 << 15), 1 << 15);
                assert_eq!(word & (1 << 31), 0);
                assert!(high == 32 || high == 64);
                decoded[i / 8] = (decoded[i / 8] << 1) | u8::from(high == 64);
            }
            assert_eq!(decoded, expected);
            assert_eq!(words[24] & 0x80008000, 0);
            assert_eq!((words[24] & 0x7fff) + (words[24] >> 16), 24000);
            assert_eq!(words[25], 0);
        }
    }
}
