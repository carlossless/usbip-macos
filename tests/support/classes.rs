//! Simulated devices for speeds and classes the loopback device does not cover.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;

use super::{
    server::{Completion, Handled, Outcome, Urb, EPIPE},
    usb::*,
};

pub const VID: u16 = 0x1209;
pub const PID_FIFO: u16 = 0x7358;
pub const PID_MOUSE: u16 = 0x7359;

/// USB/IP speed codes.
pub const LOW: u32 = 1;
pub const FULL: u32 = 2;
pub const HIGH: u32 = 3;
pub const SUPER: u32 = 5;
pub const SUPER_PLUS: u32 = 6;

/// Bulk OUT 0x01 feeds a FIFO that bulk IN 0x81 drains.
#[derive(Default)]
pub struct Fifo {
    data: Mutex<VecDeque<u8>>,
    ready: Notify,
    pub received: Mutex<usize>,
}

impl Function for Fifo {
    fn transfer(self: Arc<Self>, urb: Urb) -> Handled {
        let this = self;
        Box::pin(async move {
            if !urb.is_in() {
                this.data.lock().unwrap().extend(&urb.data);
                *this.received.lock().unwrap() += urb.data.len();
                this.ready.notify_waiters();
                return Outcome::Complete(Completion::written(urb.data.len() as u32));
            }
            loop {
                let ready = this.ready.notified();
                {
                    let mut data = this.data.lock().unwrap();
                    if !data.is_empty() {
                        let n = data.len().min(urb.length as usize);
                        return Outcome::Complete(Completion::ok(data.drain(..n).collect()));
                    }
                }
                ready.await;
            }
        })
    }
}

/// A bulk FIFO with descriptors valid for `speed`: 64-byte bulk packets at full speed, 512 at
/// high speed, 1024 plus companion descriptors and a BOS at SuperSpeed.
pub fn fifo(speed: u32) -> StdDevice<Fifo> {
    let (bcd_usb, mps0, bulk) = match speed {
        SUPER | SUPER_PLUS => (0x0320, 9, 1024),
        HIGH => (0x0200, 64, 512),
        _ => (0x0200, 64, 64),
    };
    let mut body = interface(0, 0, 2, [0xff, 0, 0]);
    for address in [0x01, 0x81] {
        body.extend(endpoint(address, 2, bulk, 0));
        if speed >= SUPER {
            body.extend(ss_companion(3, 0, 0));
        }
    }
    StdDevice::new(
        Descriptors {
            device: device_descriptor(bcd_usb, [0, 0, 0], mps0, VID, PID_FIFO, 1),
            configurations: vec![configuration(1, &body)],
            strings: vec![
                "usbip-macos tests".into(),
                "FIFO".into(),
                format!("FIFO{speed}"),
            ],
            bos: (speed >= SUPER).then(superspeed_bos),
            qualifier: None,
            ..Default::default()
        },
        Fifo::default(),
    )
}

/// A low-speed boot mouse that reports no movement.
#[derive(Default)]
pub struct Mouse {
    pub polls: Mutex<usize>,
}

const MOUSE_REPORT: [u8; 50] = [
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0xc0, 0xc0,
];

impl Function for Mouse {
    fn control(&self, urb: &Urb) -> Option<Outcome> {
        match (urb.request_type(), urb.request()) {
            // SET_IDLE, SET_PROTOCOL
            (0x21, 0x0a | 0x0b) => Some(ack()),
            // GET_REPORT
            (0xa1, 0x01) => Some(reply(vec![0; 3], urb)),
            _ => None,
        }
    }

    fn transfer(self: Arc<Self>, urb: Urb) -> Handled {
        let this = self;
        Box::pin(async move {
            if !urb.is_in() {
                return Outcome::Complete(Completion::status(-EPIPE));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            *this.polls.lock().unwrap() += 1;
            Outcome::Complete(Completion::ok(vec![0; 3]))
        })
    }
}

pub fn low_speed_mouse() -> StdDevice<Mouse> {
    let mut body = interface(0, 0, 1, [3, 1, 2]);
    let len = (MOUSE_REPORT.len() as u16).to_le_bytes();
    body.extend([9, 0x21, 0x11, 0x01, 0, 1, 0x22, len[0], len[1]]);
    body.extend(endpoint(0x81, 3, 4, 10));
    StdDevice::new(
        Descriptors {
            device: device_descriptor(0x0110, [0, 0, 0], 8, VID, PID_MOUSE, 1),
            configurations: vec![configuration(1, &body)],
            strings: vec!["usbip-macos tests".into(), "Mouse".into(), "MOUSE1".into()],
            hid_reports: [(0, MOUSE_REPORT.to_vec())].into(),
            ..Default::default()
        },
        Mouse::default(),
    )
}

pub const PID_BURST: u16 = 0x735b;

/// Interrupt IN data, numbered so a test can tell reports apart.
#[derive(Default)]
pub struct Burst {
    pub reports: Mutex<u8>,
}

impl Function for Burst {
    fn transfer(self: Arc<Self>, urb: Urb) -> Handled {
        let this = self;
        Box::pin(async move {
            if !urb.is_in() {
                return Outcome::Complete(Completion::status(-EPIPE));
            }
            let n = {
                let mut reports = this.reports.lock().unwrap();
                *reports = reports.wrapping_add(1);
                *reports
            };
            let data = (0..urb.length).map(|i| n.wrapping_add(i as u8)).collect();
            Outcome::Complete(Completion::ok(data))
        })
    }
}

/// A high-speed, high-bandwidth interrupt IN endpoint: three 1024-byte packets per microframe
/// (`wMaxPacketSize` 0x1400), as some video and audio devices use.
pub fn high_bandwidth_interrupt() -> StdDevice<Burst> {
    let mut body = interface(0, 0, 1, [0xff, 0, 0]);
    body.extend(endpoint(0x81, 3, 0x1400, 1));
    StdDevice::new(
        Descriptors {
            device: device_descriptor(0x0200, [0, 0, 0], 64, VID, PID_BURST, 1),
            configurations: vec![configuration(1, &body)],
            strings: vec!["usbip-macos tests".into(), "Burst".into(), "BURST1".into()],
            ..Default::default()
        },
        Burst::default(),
    )
}

pub const PID_MULTI: u16 = 0x735a;

/// Two configurations: 1 has a bulk FIFO interface, 2 a single interrupt interface.
pub fn two_configurations() -> StdDevice<Fifo> {
    let mut first = interface(0, 0, 2, [0xff, 0, 0]);
    first.extend(endpoint(0x01, 2, 512, 0));
    first.extend(endpoint(0x81, 2, 512, 0));
    let mut second = interface(0, 0, 1, [0xff, 1, 0]);
    second.extend(endpoint(0x82, 3, 64, 4));
    StdDevice::new(
        Descriptors {
            device: device_descriptor(0x0200, [0, 0, 0], 64, VID, PID_MULTI, 2),
            configurations: vec![configuration(1, &first), configuration(2, &second)],
            strings: vec![
                "usbip-macos tests".into(),
                "Two configs".into(),
                "MULTI1".into(),
            ],
            ..Default::default()
        },
        Fifo::default(),
    )
}
