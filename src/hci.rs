//! Platform independent view of the IOUSBHostControllerInterface message format.
//!
//! The bit layouts mirror `IOUSBHostControllerInterfaceDefinitions.h`; `host.rs` has tests
//! asserting they match the values exported by the SDK.

pub const CONTROL_TYPE_MASK: u32 = 0x3f;
pub const CONTROL_STATUS_SHIFT: u32 = 8;
pub const CONTROL_STATUS_MASK: u32 = 0xf << CONTROL_STATUS_SHIFT;
pub const CONTROL_NO_RESPONSE: u32 = 1 << 14;
pub const CONTROL_VALID: u32 = 1 << 15;
pub const ISO_CONTROL_FRAME_SHIFT: u32 = 16;
pub const ISO_CONTROL_FRAME_MASK: u32 = 0xff << ISO_CONTROL_FRAME_SHIFT;
pub const ISO_CONTROL_ASAP: u32 = 1 << 24;
pub const TRANSFER_LENGTH_MASK: u32 = 0x0fff_ffff;

pub mod message_type {
    pub const CONTROLLER_CAPABILITIES: u32 = 0x00;
    pub const PORT_CAPABILITIES: u32 = 0x01;
    pub const CONTROLLER_POWER_ON: u32 = 0x10;
    pub const CONTROLLER_POWER_OFF: u32 = 0x11;
    pub const CONTROLLER_START: u32 = 0x12;
    pub const CONTROLLER_PAUSE: u32 = 0x13;
    pub const CONTROLLER_FRAME_NUMBER: u32 = 0x14;
    pub const PORT_POWER_ON: u32 = 0x18;
    pub const PORT_POWER_OFF: u32 = 0x19;
    pub const PORT_RESUME: u32 = 0x1a;
    pub const PORT_SUSPEND: u32 = 0x1b;
    pub const PORT_RESET: u32 = 0x1c;
    pub const PORT_DISABLE: u32 = 0x1d;
    pub const PORT_STATUS: u32 = 0x1e;
    pub const DEVICE_CREATE: u32 = 0x20;
    pub const DEVICE_DESTROY: u32 = 0x21;
    pub const DEVICE_START: u32 = 0x22;
    pub const DEVICE_PAUSE: u32 = 0x23;
    pub const DEVICE_UPDATE: u32 = 0x24;
    pub const ENDPOINT_CREATE: u32 = 0x28;
    pub const ENDPOINT_DESTROY: u32 = 0x29;
    pub const ENDPOINT_PAUSE: u32 = 0x2b;
    pub const ENDPOINT_UPDATE: u32 = 0x2c;
    pub const ENDPOINT_RESET: u32 = 0x2d;
    pub const ENDPOINT_SET_NEXT_TRANSFER: u32 = 0x2e;
    pub const SETUP_TRANSFER: u32 = 0x38;
    pub const NORMAL_TRANSFER: u32 = 0x39;
    pub const STATUS_TRANSFER: u32 = 0x3a;
    pub const ISOCHRONOUS_TRANSFER: u32 = 0x3b;
    pub const LINK: u32 = 0x3c;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawMessage {
    pub control: u32,
    pub data0: u32,
    pub data1: u64,
}

impl RawMessage {
    pub fn message_type(&self) -> u32 {
        self.control & CONTROL_TYPE_MASK
    }

    pub fn is_valid(&self) -> bool {
        self.control & CONTROL_VALID != 0
    }

    pub fn needs_response(&self) -> bool {
        self.control & CONTROL_NO_RESPONSE == 0
    }

    pub fn device_address(&self) -> u8 {
        self.data0 as u8
    }

    pub fn endpoint_address(&self) -> u8 {
        (self.data0 >> 8) as u8
    }

    pub fn transfer_length(&self) -> u32 {
        self.data0 & TRANSFER_LENGTH_MASK
    }

    pub fn buffer(&self) -> u64 {
        self.data1
    }
}

/// A transfer ring entry, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    Setup([u8; 8]),
    Normal { length: u32 },
    Status,
    Isochronous { length: u32, frame: Option<u8> },
    Link { needs_response: bool },
    Other(u32),
}

impl Transfer {
    /// `None` when the ring has no valid message to process.
    pub fn decode(msg: &RawMessage) -> Option<Self> {
        use message_type::*;
        if !msg.is_valid() {
            return None;
        }
        Some(match msg.message_type() {
            SETUP_TRANSFER => Self::Setup(msg.data1.to_le_bytes()),
            NORMAL_TRANSFER => Self::Normal {
                length: msg.transfer_length(),
            },
            STATUS_TRANSFER => Self::Status,
            ISOCHRONOUS_TRANSFER => Self::Isochronous {
                length: msg.transfer_length(),
                frame: (msg.control & ISO_CONTROL_ASAP == 0).then_some(
                    ((msg.control & ISO_CONTROL_FRAME_MASK) >> ISO_CONTROL_FRAME_SHIFT) as u8,
                ),
            },
            LINK => Self::Link {
                needs_response: msg.needs_response(),
            },
            other => Self::Other(other),
        })
    }
}

/// Doorbell bit layout: device address in bits 0-7, endpoint address in 8-15, stream in 16-31.
pub fn decode_doorbell(doorbell: u32) -> (u8, u8, u16) {
    (
        doorbell as u8,
        (doorbell >> 8) as u8,
        (doorbell >> 16) as u16,
    )
}

/// Completion status reported back to the host controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Success,
    Offline,
    Timeout,
    EndpointStopped,
    ProtocolError,
    TransactionError,
    OverrunError,
    StallError,
    Error,
}

impl Status {
    /// Maps a Linux URB status (negative errno) from RET_SUBMIT.
    pub fn from_urb_status(status: i32) -> Self {
        match -status {
            0 => Self::Success,
            // -EREMOTEIO: short packet with URB_SHORT_NOT_OK; -EXDEV: partially completed iso.
            121 | 18 => Self::Success,
            32 => Self::StallError,
            75 => Self::OverrunError,
            71 | 84 => Self::ProtocolError,
            62 | 110 => Self::Timeout,
            19 | 108 => Self::Offline,
            2 | 104 => Self::EndpointStopped,
            70 | 63 => Self::TransactionError,
            _ => Self::Error,
        }
    }
}

/// Lowest-free allocation of USB device addresses (1..=127).
#[derive(Debug, Default)]
pub struct AddressPool {
    used: u128,
}

impl AddressPool {
    pub fn allocate(&mut self) -> Option<u8> {
        let address = (1..128u8).find(|&a| self.used & (1 << a) == 0)?;
        self.used |= 1 << address;
        Some(address)
    }

    pub fn release(&mut self, address: u8) {
        if address < 128 {
            self.used &= !(1 << address);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

/// The parts of a standard endpoint descriptor the engine needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointInfo {
    pub kind: EndpointKind,
    pub max_packet_size: u16,
    pub b_interval: u8,
}

impl EndpointInfo {
    pub const CONTROL: Self = Self {
        kind: EndpointKind::Control,
        max_packet_size: 64,
        b_interval: 0,
    };

    /// Parses a standard endpoint descriptor (`bLength` 7, `bDescriptorType` 5), or the first one
    /// inside a configuration descriptor, which is how macOS passes non-default endpoints.
    pub fn parse(desc: &[u8]) -> Option<Self> {
        if desc.len() >= 4 && desc[0] >= 2 && desc[1] == 2 {
            let total = (u16::from_le_bytes([desc[2], desc[3]]) as usize).min(desc.len());
            let mut i = desc[0] as usize;
            while i + 2 <= total && desc[i] >= 2 {
                if desc[i + 1] == 5 {
                    return Self::parse(desc.get(i..total)?);
                }
                i += desc[i] as usize;
            }
            return None;
        }
        if desc.len() < 7 || desc[1] != 5 {
            return None;
        }
        Some(Self {
            kind: match desc[3] & 0x03 {
                0 => EndpointKind::Control,
                1 => EndpointKind::Isochronous,
                2 => EndpointKind::Bulk,
                _ => EndpointKind::Interrupt,
            },
            max_packet_size: u16::from_le_bytes([desc[4], desc[5]]),
            b_interval: desc[6],
        })
    }

    /// The URB interval Linux's `usb_fill_int_urb` derives from `bInterval`: microframes as a
    /// power of two at high speed and above, frames otherwise. Linux rejects periodic URBs with
    /// an interval of 0, and its USB/IP server drops the connection when that happens.
    pub fn urb_interval(&self, speed: crate::protocol::Speed) -> u32 {
        use crate::protocol::Speed;
        let b = u32::from(self.b_interval.max(1));
        match (self.kind, speed) {
            (EndpointKind::Bulk | EndpointKind::Control, _) => 0,
            (_, Speed::High | Speed::Wireless | Speed::Super | Speed::SuperPlus) => {
                1 << (b.min(16) - 1)
            }
            (EndpointKind::Isochronous, _) => 1 << (b.min(16) - 1),
            (EndpointKind::Interrupt, _) => b,
        }
    }
}

/// Removes UAS alternate settings from a configuration descriptor when the same interface has
/// another alternate setting to fall back to, which for disks is Bulk-Only Transport. UAS needs
/// bulk streams, which neither USB/IP nor `IOUSBHostControllerInterface` supports, and macOS
/// does not fall back on its own. Leaves the descriptor alone if it is malformed. Returns the
/// original and new `wTotalLength` when it hides something.
pub fn hide_uas(config: &mut Vec<u8>) -> Option<(u16, u16)> {
    if config.len() < 9 || config[0] < 9 || config[1] != 2 {
        return None;
    }
    // Byte ranges of each interface descriptor and the descriptors that belong to it.
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    let mut i = config[0] as usize;
    while i < config.len() {
        let len = config[i] as usize;
        if len < 2 || i + len > config.len() {
            return None;
        }
        match config[i + 1] {
            4 => groups.push(i..i + len),
            // An interface association starts a new function.
            0x0b => groups.push(i..i),
            _ => {
                if let Some(g) = groups.last_mut().filter(|g| g.start != g.end) {
                    g.end = i + len;
                }
            }
        }
        i += len;
    }
    let is_uas = |g: &std::ops::Range<usize>| {
        g.len() >= 9 && config[g.start + 5..g.start + 8] == [0x08, 0x06, 0x62]
    };
    let fallback = |number: u8| {
        groups
            .iter()
            .any(|g| g.len() >= 9 && config[g.start + 2] == number && !is_uas(g))
    };
    let hidden: Vec<_> = groups
        .iter()
        .filter(|g| is_uas(g) && fallback(config[g.start + 2]))
        .cloned()
        .collect();
    if hidden.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(config.len());
    let mut at = 0;
    for g in &hidden {
        out.extend_from_slice(&config[at..g.start]);
        at = g.end;
    }
    out.extend_from_slice(&config[at..]);
    let total = (out.len() as u16).to_le_bytes();
    out[2..4].copy_from_slice(&total);
    let original = u16::from_le_bytes([config[2], config[3]]);
    *config = out;
    Some((original, u16::from_le_bytes(total)))
}

#[cfg(test)]
mod uas_tests {
    use super::hide_uas;

    fn interface(number: u8, alt: u8, endpoints: u8, protocol: u8) -> Vec<u8> {
        vec![9, 4, number, alt, endpoints, 0x08, 0x06, protocol, 0]
    }

    fn config(body: &[u8]) -> Vec<u8> {
        let total = (9 + body.len()) as u16;
        let mut d = vec![9, 2, total as u8, (total >> 8) as u8, 1, 1, 0, 0x80, 50];
        d.extend_from_slice(body);
        d
    }

    #[test]
    fn hides_uas_when_bulk_only_is_there() {
        let bot: Vec<u8> = [
            interface(0, 0, 2, 0x50),
            vec![7, 5, 0x81, 2, 0, 4, 0],
            vec![6, 0x30, 15, 0, 0, 0],
        ]
        .concat();
        let uas: Vec<u8> = [
            interface(0, 1, 4, 0x62),
            vec![7, 5, 0x81, 2, 0, 4, 0],
            vec![6, 0x30, 15, 5, 0, 0],
            vec![4, 0x24, 3, 0],
        ]
        .concat();
        let mut d = config(&[bot.clone(), uas].concat());
        hide_uas(&mut d);
        assert_eq!(d, config(&bot));
    }

    #[test]
    fn keeps_a_uas_only_interface() {
        let original = config(&[interface(0, 0, 4, 0x62), vec![7, 5, 0x81, 2, 0, 4, 0]].concat());
        let mut d = original.clone();
        hide_uas(&mut d);
        assert_eq!(d, original);
    }

    #[test]
    fn leaves_other_interfaces_and_short_reads_alone() {
        let body = [
            interface(0, 0, 2, 0x50),
            interface(0, 1, 4, 0x62),
            vec![9, 4, 1, 0, 1, 3, 0, 0, 0],
            vec![7, 5, 0x83, 3, 8, 0, 4],
        ]
        .concat();
        let mut d = config(&body);
        hide_uas(&mut d);
        let expected = config(
            &[
                interface(0, 0, 2, 0x50),
                vec![9, 4, 1, 0, 1, 3, 0, 0, 0],
                vec![7, 5, 0x83, 3, 8, 0, 4],
            ]
            .concat(),
        );
        assert_eq!(d, expected);
        let mut header = config(&body)[..9].to_vec();
        let before = header.clone();
        hide_uas(&mut header);
        assert_eq!(header, before, "a header-only read has nothing to hide");
    }
}
