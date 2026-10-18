//! Bridge the radio driver's packet interface to the polled smoltcp stack.
use core::{
    future::Future,
    task::{Context, Poll, Waker},
};
use esp_radio::wifi::{Interface, WifiRxToken, WifiTxToken};
use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};

pub fn current_millis() -> u64 {
    esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis()
}

/// Poll radio operations without starving button sampling or LED updates.
/// The RTOS preempts this loop to run the Wi-Fi driver threads.
pub fn wait_with_tick<F: Future>(
    future: F,
    timeout_ms: u64,
    mut tick: impl FnMut(),
) -> Option<F::Output> {
    let mut future = core::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    let deadline = current_millis() + timeout_ms;
    loop {
        tick();
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            return Some(result);
        }
        if current_millis() >= deadline {
            return None;
        }
    }
}

pub struct DeviceAdapter(pub Interface);
pub struct Rx(WifiRxToken);
pub struct Tx(WifiTxToken);

impl Device for DeviceAdapter {
    type RxToken<'a>
        = Rx
    where
        Self: 'a;
    type TxToken<'a>
        = Tx
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Rx, Tx)> {
        self.0.receive().map(|(rx, tx)| (Rx(rx), Tx(tx)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Tx> {
        self.0.transmit().map(Tx)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        let radio_caps = embassy_net_driver::Driver::capabilities(&self.0);
        caps.max_transmission_unit = radio_caps.max_transmission_unit;
        caps.max_burst_size = radio_caps.max_burst_size;
        caps
    }
}

impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        self.0.consume_token(|buffer| f(buffer))
    }
}

impl TxToken for Tx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.0.consume_token(len, f)
    }
}
