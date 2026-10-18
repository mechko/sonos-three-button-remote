pub struct ButtonState {
    stable: bool,
    candidate: bool,
    changed_at: u64,
    pending: u8,
}

impl ButtonState {
    pub fn new(initial: bool, now: u64) -> Self {
        Self {
            stable: initial,
            candidate: initial,
            changed_at: now,
            pending: 0,
        }
    }
    pub fn update(&mut self, reading: bool, now: u64) {
        if reading != self.candidate {
            self.candidate = reading;
            self.changed_at = now;
        }
        if reading != self.stable && now.saturating_sub(self.changed_at) >= 20 {
            self.stable = reading;
            if reading {
                self.pending = self.pending.saturating_add(1);
            }
        }
    }
    pub fn take_press(&mut self) -> bool {
        if self.pending == 0 {
            return false;
        }
        self.pending -= 1;
        true
    }
}

/// Preserve station selections made while networking is busy. If full,
/// discard the oldest selection so the most recent choice is retained.
pub struct Buttons {
    states: [ButtonState; 3],
    queue: [usize; 16],
    head: usize,
    len: usize,
}

impl Buttons {
    pub fn new(readings: [bool; 3], now: u64) -> Self {
        Self {
            states: readings.map(|reading| ButtonState::new(reading, now)),
            queue: [0; 16],
            head: 0,
            len: 0,
        }
    }
    pub fn update(&mut self, readings: [bool; 3], now: u64) {
        for (index, reading) in readings.into_iter().enumerate() {
            self.states[index].update(reading, now);
            if self.states[index].take_press() {
                if self.len == self.queue.len() {
                    self.head = (self.head + 1) % self.queue.len();
                    self.len -= 1;
                }
                self.queue[(self.head + self.len) % self.queue.len()] = index;
                self.len += 1;
            }
        }
    }
    pub fn has_press(&self) -> bool {
        self.len != 0
    }
    pub fn take_press(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let index = self.queue[self.head];
        self.head = (self.head + 1) % self.queue.len();
        self.len -= 1;
        Some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multiple_buttons_queue_stations_in_press_order_without_hold_repeats() {
        let mut buttons = Buttons::new([false; 3], 0);
        for (base, index) in [(10, 2), (100, 0), (200, 1)] {
            let mut readings = [false; 3];
            readings[index] = true;
            buttons.update(readings, base);
            buttons.update(readings, base + 20);
            buttons.update(readings, base + 40);
            buttons.update([false; 3], base + 50);
            buttons.update([false; 3], base + 70);
        }
        assert!(buttons.has_press());
        assert_eq!(buttons.take_press(), Some(2));
        assert_eq!(buttons.take_press(), Some(0));
        assert_eq!(buttons.take_press(), Some(1));
        assert_eq!(buttons.take_press(), None);
    }

    #[test]
    fn bounce_and_holding_do_not_repeat_but_repress_does() {
        let mut button = ButtonState::new(false, 0);
        for (reading, now) in [(true, 1), (false, 5), (true, 8), (true, 27)] {
            button.update(reading, now);
            assert!(!button.take_press());
        }
        button.update(true, 28);
        assert!(button.take_press());
        button.update(true, 1000);
        assert!(!button.take_press());
        button.update(false, 1001);
        button.update(false, 1021);
        assert!(!button.take_press());
        button.update(true, 1030);
        button.update(true, 1050);
        assert!(button.take_press());
    }
    #[test]
    fn queued_presses_survive_network_work_and_boot_hold_is_not_a_press() {
        let mut button = ButtonState::new(true, 0);
        button.update(true, 100);
        assert!(!button.take_press());
        for base in [200, 300] {
            button.update(false, base);
            button.update(false, base + 20);
            button.update(true, base + 40);
            button.update(true, base + 60);
        }
        assert!(button.take_press());
        assert!(button.take_press());
        assert!(!button.take_press());
    }
}
