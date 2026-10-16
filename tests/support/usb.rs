//! Building blocks for simulated USB devices: descriptor builders and a device that answers the
//! standard requests (USB 2.0 chapter 9) itself and hands everything else to a [`Function`].

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use super::server::{Completion, Device, Export, Handled, Outcome, Urb, EPIPE};

pub fn device_descriptor(
    bcd_usb: u16,
    class: [u8; 3],
    mps0: u8,
    vid: u16,
    pid: u16,
    configs: u8,
) -> Vec<u8> {
    let mut d = vec![18, 1];
    d.extend_from_slice(&bcd_usb.to_le_bytes());
    d.extend_from_slice(&class);
    d.push(mps0);
    d.extend_from_slice(&vid.to_le_bytes());
    d.extend_from_slice(&pid.to_le_bytes());
    d.extend_from_slice(&[0x00, 0x01, 1, 2, 3, configs]);
    d
}

pub fn interface(number: u8, alt: u8, endpoints: u8, class: [u8; 3]) -> Vec<u8> {
    vec![
        9, 4, number, alt, endpoints, class[0], class[1], class[2], 0,
    ]
}

pub fn endpoint(address: u8, attributes: u8, max_packet: u16, interval: u8) -> Vec<u8> {
    let mp = max_packet.to_le_bytes();
    vec![7, 5, address, attributes, mp[0], mp[1], interval]
}

pub fn ss_companion(max_burst: u8, attributes: u8, bytes_per_interval: u16) -> Vec<u8> {
    let b = bytes_per_interval.to_le_bytes();
    vec![6, 0x30, max_burst, attributes, b[0], b[1]]
}

pub fn iad(first: u8, count: u8, class: [u8; 3]) -> Vec<u8> {
    vec![8, 0x0b, first, count, class[0], class[1], class[2], 0]
}

/// A configuration descriptor around `body`; `bNumInterfaces` counts distinct interface numbers.
pub fn configuration(value: u8, body: &[u8]) -> Vec<u8> {
    let mut interfaces = HashSet::new();
    let mut i = 0;
    while i + 1 < body.len() && body[i] > 0 {
        if body[i + 1] == 4 {
            interfaces.insert(body[i + 2]);
        }
        i += body[i] as usize;
    }
    let total = (9 + body.len()) as u16;
    let mut d = vec![9, 2];
    d.extend_from_slice(&total.to_le_bytes());
    d.extend_from_slice(&[interfaces.len() as u8, value, 0, 0x80, 50]);
    d.extend_from_slice(body);
    d
}

pub fn string_descriptor(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut d = vec![(2 + units.len() * 2) as u8, 3];
    for u in units {
        d.extend_from_slice(&u.to_le_bytes());
    }
    d
}

/// BOS with a USB 2.0 extension (LPM) and a SuperSpeed device capability.
pub fn superspeed_bos() -> Vec<u8> {
    let usb2 = [7, 0x10, 0x02, 0x02, 0, 0, 0];
    let ss = [10, 0x10, 0x03, 0x00, 0x0e, 0x00, 0x01, 0x0a, 0xff, 0x07];
    let mut d = vec![5, 0x0f];
    d.extend_from_slice(&((5 + usb2.len() + ss.len()) as u16).to_le_bytes());
    d.push(2);
    d.extend_from_slice(&usb2);
    d.extend_from_slice(&ss);
    d
}

#[derive(Clone, Default)]
pub struct Descriptors {
    pub device: Vec<u8>,
    pub configurations: Vec<Vec<u8>>,
    /// Indexed from 1: manufacturer, product, serial, then any others.
    pub strings: Vec<String>,
    pub bos: Option<Vec<u8>>,
    pub qualifier: Option<Vec<u8>>,
    /// HID report descriptors by interface number.
    pub hid_reports: HashMap<u8, Vec<u8>>,
}

impl Descriptors {
    pub fn vid(&self) -> u16 {
        u16::from_le_bytes([self.device[8], self.device[9]])
    }

    pub fn pid(&self) -> u16 {
        u16::from_le_bytes([self.device[10], self.device[11]])
    }

    fn for_each<F: FnMut(&[u8])>(config: &[u8], mut f: F) {
        let mut i = 0;
        while i + 1 < config.len() && config[i] > 0 {
            f(&config[i..i + config[i] as usize]);
            i += config[i] as usize;
        }
    }

    /// `(bEndpointAddress, bmAttributes)` of every endpoint in every configuration.
    pub fn endpoints(&self) -> Vec<(u8, u8)> {
        let mut out = Vec::new();
        for c in &self.configurations {
            Self::for_each(c, |d| {
                if d[1] == 5 && !out.iter().any(|e: &(u8, u8)| e.0 == d[2]) {
                    out.push((d[2], d[3]));
                }
            });
        }
        out
    }

    fn interfaces(&self) -> Vec<[u8; 3]> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        if let Some(c) = self.configurations.first() {
            Self::for_each(c, |d| {
                if d[1] == 4 && seen.insert(d[2]) {
                    out.push([d[5], d[6], d[7]]);
                }
            });
        }
        out
    }
}

#[derive(Debug, Default)]
pub struct StdState {
    pub configuration: u8,
    pub alternates: HashMap<u8, u8>,
    pub halted: HashSet<u8>,
}

/// Device specific behaviour on top of [`StdDevice`].
pub trait Function: Send + Sync + 'static {
    /// Class and vendor control requests. `None` stalls.
    fn control(&self, _urb: &Urb) -> Option<Outcome> {
        None
    }
    /// Transfers on every endpoint but 0.
    fn transfer(self: Arc<Self>, urb: Urb) -> Handled;
    fn set_interface(&self, _interface: u8, _alt: u8) {}
    fn set_configuration(&self, _value: u8) {}
}

pub struct StdDevice<F> {
    pub descriptors: Arc<Descriptors>,
    pub function: Arc<F>,
    pub state: Arc<Mutex<StdState>>,
}

impl<F> Clone for StdDevice<F> {
    fn clone(&self) -> Self {
        Self {
            descriptors: Arc::clone(&self.descriptors),
            function: Arc::clone(&self.function),
            state: Arc::clone(&self.state),
        }
    }
}

pub fn reply(data: Vec<u8>, urb: &Urb) -> Outcome {
    let mut data = data;
    data.truncate(urb.w_length() as usize);
    Outcome::Complete(Completion::ok(data))
}

pub fn ack() -> Outcome {
    Outcome::Complete(Completion::written(0))
}

pub fn stall() -> Outcome {
    Outcome::Complete(Completion::status(-EPIPE))
}

impl<F: Function> StdDevice<F> {
    pub fn new(descriptors: Descriptors, function: F) -> Self {
        Self {
            descriptors: Arc::new(descriptors),
            function: Arc::new(function),
            state: Arc::default(),
        }
    }

    pub fn export(&self, busid: &str, speed: u32) -> Export {
        let mut e = Export::new(busid, self.clone());
        let d = &self.descriptors.device;
        e.id_vendor = self.descriptors.vid();
        e.id_product = self.descriptors.pid();
        e.bcd_device = u16::from_le_bytes([d[12], d[13]]);
        e.class = [d[4], d[5], d[6]];
        e.num_configurations = d[17];
        e.speed = speed;
        e.interfaces = self.descriptors.interfaces();
        e
    }

    pub fn configuration(&self) -> u8 {
        self.state.lock().unwrap().configuration
    }

    fn standard(&self, urb: &Urb) -> Option<Outcome> {
        let d = &self.descriptors;
        let mut s = self.state.lock().unwrap();
        Some(match (urb.request_type(), urb.request()) {
            (0x80, 6) => {
                let [index, kind] = urb.value().to_le_bytes();
                let data = match kind {
                    1 => Some(d.device.clone()),
                    2 => d.configurations.get(index as usize).cloned(),
                    3 if index == 0 => Some(vec![4, 3, 0x09, 0x04]),
                    3 => d
                        .strings
                        .get(index as usize - 1)
                        .map(|s| string_descriptor(s)),
                    6 => d.qualifier.clone(),
                    0x0f => d.bos.clone(),
                    _ => None,
                };
                match data {
                    Some(data) => reply(data, urb),
                    None => stall(),
                }
            }
            (0x81, 6) => {
                let [_, kind] = urb.value().to_le_bytes();
                match d.hid_reports.get(&(urb.index() as u8)) {
                    Some(r) if kind == 0x22 => reply(r.clone(), urb),
                    _ => stall(),
                }
            }
            (0x00, 5) => ack(),
            (0x00, 9) => {
                let value = urb.value() as u8;
                if value != 0 && !d.configurations.iter().any(|c| c[5] == value) {
                    return Some(stall());
                }
                s.configuration = value;
                s.alternates.clear();
                s.halted.clear();
                drop(s);
                self.function.set_configuration(value);
                ack()
            }
            (0x80, 8) => reply(vec![s.configuration], urb),
            (0x80 | 0x81, 0) => reply(vec![0, 0], urb),
            (0x82, 0) => {
                let halted = s.halted.contains(&(urb.index() as u8));
                reply(vec![halted as u8, 0], urb)
            }
            (0x01, 11) => {
                let (iface, alt) = (urb.index() as u8, urb.value() as u8);
                s.alternates.insert(iface, alt);
                drop(s);
                self.function.set_interface(iface, alt);
                ack()
            }
            (0x81, 10) => reply(
                vec![*s.alternates.get(&(urb.index() as u8)).unwrap_or(&0)],
                urb,
            ),
            (0x02, 1) => {
                s.halted.remove(&(urb.index() as u8));
                ack()
            }
            (0x02, 3) => {
                s.halted.insert(urb.index() as u8);
                ack()
            }
            (0x00, 1 | 3) | (0x01, 1 | 3) => ack(),
            (0x00, 0x30 | 0x31) => ack(),
            _ => return None,
        })
    }

    pub fn halt(&self, endpoint: u8) {
        self.state.lock().unwrap().halted.insert(endpoint);
    }
}

impl<F: Function> Device for StdDevice<F> {
    fn submit(&self, urb: Urb) -> Handled {
        if urb.ep == 0 {
            let outcome = self
                .standard(&urb)
                .or_else(|| self.function.control(&urb))
                .unwrap_or_else(stall);
            return Box::pin(async move { outcome });
        }
        let address = urb.ep as u8 | if urb.is_in() { 0x80 } else { 0 };
        if self.state.lock().unwrap().halted.contains(&address) {
            return Box::pin(async { stall() });
        }
        Arc::clone(&self.function).transfer(urb)
    }

    fn endpoints(&self) -> Vec<(u8, u8)> {
        self.descriptors.endpoints()
    }

    fn reset(&self) {
        *self.state.lock().unwrap() = StdState::default();
    }
}
