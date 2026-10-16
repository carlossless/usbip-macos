//! A simulated vendor-specific loopback device, complete enough for macOS to enumerate.
//!
//! Interface 0: bulk OUT 0x01 feeds a FIFO that bulk IN 0x81 drains; interrupt IN 0x82 counts.
//! Interface 1, alt 1: isochronous IN 0x83 produces a pattern, isochronous OUT 0x03 sinks.
//! Vendor control requests are listed on [`LoopbackDevice::submit`].

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::Notify;

use super::server::{Completion, Device, Export, Handled, IsoDesc, Outcome, Urb, EPIPE};

pub const VID: u16 = 0x1209;
pub const PID: u16 = 0x7357;
pub const MANUFACTURER: &str = "usbip-macos tests";
pub const PRODUCT: &str = "Loopback";
pub const SERIAL: &str = "LB0001";

pub const REQ_WRITE: u8 = 0x01;
pub const REQ_READ: u8 = 0x02;
pub const REQ_STALL: u8 = 0x03;
pub const REQ_HALT: u8 = 0x04;
pub const REQ_PATTERN: u8 = 0x05;
pub const REQ_SHORT: u8 = 0x06;
pub const REQ_DISCONNECT: u8 = 0x07;

pub const EP_BULK_OUT: u8 = 0x01;
pub const EP_BULK_IN: u8 = 0x81;
pub const EP_INT_IN: u8 = 0x82;
pub const EP_ISO_IN: u8 = 0x83;
pub const EP_ISO_OUT: u8 = 0x03;

pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn device_descriptor() -> Vec<u8> {
    let mut d = vec![18, 1, 0x00, 0x02, 0x00, 0x00, 0x00, 64];
    d.extend_from_slice(&VID.to_le_bytes());
    d.extend_from_slice(&PID.to_le_bytes());
    d.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]);
    d
}

fn endpoint(address: u8, attributes: u8, max_packet: u16, interval: u8) -> [u8; 7] {
    let mp = max_packet.to_le_bytes();
    [7, 5, address, attributes, mp[0], mp[1], interval]
}

fn configuration_descriptor() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[9, 4, 0, 0, 3, 0xff, 0, 0, 0]);
    body.extend_from_slice(&endpoint(EP_BULK_OUT, 2, 512, 0));
    body.extend_from_slice(&endpoint(EP_BULK_IN, 2, 512, 0));
    body.extend_from_slice(&endpoint(EP_INT_IN, 3, 64, 4));
    body.extend_from_slice(&[9, 4, 1, 0, 0, 0xff, 0, 0, 0]);
    body.extend_from_slice(&[9, 4, 1, 1, 2, 0xff, 0, 0, 0]);
    body.extend_from_slice(&endpoint(EP_ISO_IN, 1, 64, 4));
    body.extend_from_slice(&endpoint(EP_ISO_OUT, 1, 64, 4));
    let total = (9 + body.len()) as u16;
    let mut d = vec![9, 2];
    d.extend_from_slice(&total.to_le_bytes());
    d.extend_from_slice(&[2, 1, 0, 0x80, 50]);
    d.extend_from_slice(&body);
    d
}

fn string_descriptor(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut d = vec![(2 + units.len() * 2) as u8, 3];
    for u in units {
        d.extend_from_slice(&u.to_le_bytes());
    }
    d
}

#[derive(Default)]
struct State {
    fifo: VecDeque<u8>,
    control: Vec<u8>,
    halted: Vec<u8>,
    configuration: u8,
    alternate: [u8; 2],
    interrupt_counter: u64,
    iso_out: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct LoopbackDevice {
    state: Arc<Mutex<State>>,
    data: Arc<Notify>,
}

impl LoopbackDevice {
    pub fn export(&self, busid: &str) -> Export {
        let mut e = Export::new(busid, self.clone());
        e.id_vendor = VID;
        e.id_product = PID;
        e.interfaces = vec![[0xff, 0, 0], [0xff, 0, 0]];
        e
    }

    pub fn iso_out(&self) -> Vec<u8> {
        self.state.lock().unwrap().iso_out.clone()
    }

    pub fn configuration(&self) -> u8 {
        self.state.lock().unwrap().configuration
    }

    fn control(&self, urb: &Urb) -> Outcome {
        let mut s = self.state.lock().unwrap();
        let w_length = urb.w_length() as usize;
        let reply = |mut data: Vec<u8>| {
            data.truncate(w_length);
            Outcome::Complete(Completion::ok(data))
        };
        let stall = Outcome::Complete(Completion::status(-EPIPE));
        match (urb.request_type(), urb.request()) {
            (0x80, 6) => {
                let [index, kind] = urb.value().to_le_bytes();
                match (kind, index) {
                    (1, 0) => reply(device_descriptor()),
                    (2, 0) => reply(configuration_descriptor()),
                    (3, 0) => reply(vec![4, 3, 0x09, 0x04]),
                    (3, 1) => reply(string_descriptor(MANUFACTURER)),
                    (3, 2) => reply(string_descriptor(PRODUCT)),
                    (3, 3) => reply(string_descriptor(SERIAL)),
                    (6, 0) => reply(vec![10, 6, 0x00, 0x02, 0, 0, 0, 64, 1, 0]),
                    _ => stall,
                }
            }
            (0x00, 9) => {
                s.configuration = urb.value() as u8;
                Outcome::Complete(Completion::written(0))
            }
            (0x80, 8) => reply(vec![s.configuration]),
            (0x80..=0x82, 0) => reply(vec![0, 0]),
            (0x01, 11) => {
                let iface = urb.index() as usize;
                if iface > 1 {
                    return stall;
                }
                s.alternate[iface] = urb.value() as u8;
                Outcome::Complete(Completion::written(0))
            }
            (0x81, 10) => match s.alternate.get(urb.index() as usize) {
                Some(&alt) => reply(vec![alt]),
                None => stall,
            },
            (0x02, 1) => {
                let ep = urb.index() as u8;
                s.halted.retain(|&h| h != ep);
                Outcome::Complete(Completion::written(0))
            }
            (0x02, 3) | (0x00, 1) | (0x00, 3) | (0x00, 5) => {
                Outcome::Complete(Completion::written(0))
            }
            (0x40, REQ_WRITE) => {
                s.control = urb.data.clone();
                Outcome::Complete(Completion::written(urb.data.len() as u32))
            }
            (0xc0, REQ_READ) => reply(s.control.clone()),
            (0xc0, REQ_STALL) | (0x40, REQ_STALL) => stall,
            (0x40, REQ_HALT) => {
                s.halted.push(urb.value() as u8);
                Outcome::Complete(Completion::written(0))
            }
            (0xc0, REQ_PATTERN) => reply(pattern(w_length, urb.value() as u8)),
            (0xc0, REQ_SHORT) => reply(pattern(urb.value() as usize, 0)),
            (_, REQ_DISCONNECT) => Outcome::Disconnect,
            _ => stall,
        }
    }

    /// Bulk IN waits for data written to bulk OUT; isochronous IN fills every packet.
    pub fn submit_urb(&self, urb: Urb) -> Handled {
        let this = self.clone();
        Box::pin(async move {
            let ep = urb.ep as u8 | if urb.is_in() { 0x80 } else { 0 };
            if urb.ep == 0 {
                return this.control(&urb);
            }
            if this.state.lock().unwrap().halted.contains(&ep) {
                return Outcome::Complete(Completion::status(-EPIPE));
            }
            match ep {
                EP_BULK_OUT => {
                    this.state.lock().unwrap().fifo.extend(&urb.data);
                    this.data.notify_waiters();
                    Outcome::Complete(Completion::written(urb.data.len() as u32))
                }
                EP_BULK_IN => loop {
                    let notified = this.data.notified();
                    {
                        let mut s = this.state.lock().unwrap();
                        if !s.fifo.is_empty() {
                            let n = s.fifo.len().min(urb.length as usize);
                            let data: Vec<u8> = s.fifo.drain(..n).collect();
                            return Outcome::Complete(Completion::ok(data));
                        }
                    }
                    notified.await;
                },
                EP_INT_IN => {
                    super::server::sleep_ms(2).await;
                    let mut s = this.state.lock().unwrap();
                    s.interrupt_counter += 1;
                    let mut data = s.interrupt_counter.to_le_bytes().to_vec();
                    data.truncate(urb.length as usize);
                    Outcome::Complete(Completion::ok(data))
                }
                EP_ISO_IN => {
                    let mut data = Vec::new();
                    let iso: Vec<IsoDesc> = urb
                        .iso
                        .iter()
                        .map(|d| {
                            data.extend(pattern(d.length as usize, d.offset as u8));
                            IsoDesc {
                                actual_length: d.length,
                                status: 0,
                                ..*d
                            }
                        })
                        .collect();
                    Outcome::Complete(Completion {
                        iso,
                        ..Completion::ok(data)
                    })
                }
                EP_ISO_OUT => {
                    this.state.lock().unwrap().iso_out.extend(&urb.data);
                    let iso = urb
                        .iso
                        .iter()
                        .map(|d| IsoDesc {
                            actual_length: d.length,
                            ..*d
                        })
                        .collect();
                    Outcome::Complete(Completion {
                        iso,
                        ..Completion::written(urb.length)
                    })
                }
                _ => Outcome::Complete(Completion::status(-EPIPE)),
            }
        })
    }
}

impl Device for LoopbackDevice {
    fn submit(&self, urb: Urb) -> Handled {
        self.submit_urb(urb)
    }

    fn reset(&self) {
        *self.state.lock().unwrap() = State::default();
    }

    fn endpoints(&self) -> Vec<(u8, u8)> {
        vec![
            (EP_BULK_OUT, 2),
            (EP_BULK_IN, 2),
            (EP_INT_IN, 3),
            (EP_ISO_IN, 1),
            (EP_ISO_OUT, 1),
        ]
    }
}
