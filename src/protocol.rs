//! Encoding and decoding of the USB/IP wire protocol (client side).
//!
//! All multi-byte fields are big endian. See the Linux kernel's
//! `Documentation/usb/usbip_protocol.rst` for the reference layout.

use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const USBIP_VERSION: u16 = 0x0111;

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

pub const USBIP_CMD_SUBMIT: u32 = 1;
pub const USBIP_CMD_UNLINK: u32 = 2;
pub const USBIP_RET_SUBMIT: u32 = 3;
pub const USBIP_RET_UNLINK: u32 = 4;

pub const URB_SHORT_NOT_OK: u32 = 0x0001;
pub const URB_ISO_ASAP: u32 = 0x0002;
pub const URB_ZERO_PACKET: u32 = 0x0040;

/// SET_FEATURE(PORT_RESET) to a hub port. `usbip-host` resets the device when it receives this
/// on endpoint 0 instead of passing it on.
pub const PORT_RESET_SETUP: [u8; 8] = [0x23, 0x03, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00];

pub const OP_HEADER_SIZE: usize = 8;
pub const BUSID_SIZE: usize = 32;
pub const PATH_SIZE: usize = 256;
pub const DEVICE_SIZE: usize = 312;
pub const INTERFACE_SIZE: usize = 4;
pub const URB_HEADER_SIZE: usize = 48;
pub const ISO_DESCRIPTOR_SIZE: usize = 16;

/// Linux caps isochronous URBs at this many packets; anything above is a corrupt stream.
pub const MAX_ISO_PACKETS: u32 = 1024;
/// Upper bound on a single transfer we are willing to buffer from the server.
pub const MAX_TRANSFER_LENGTH: u32 = 16 * 1024 * 1024;
pub const MAX_EXPORTED_DEVICES: u32 = 4096;

pub const ECONNRESET: i32 = 104;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected reply code {actual:#06x}, expected {expected:#06x}")]
    UnexpectedReply { expected: u16, actual: u16 },
    #[error("server returned error status {0}")]
    Status(u32),
    #[error("unexpected command {0:#x} from server")]
    UnexpectedCommand(u32),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ProtocolError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Direction {
    Out = 0,
    In = 1,
}

impl Direction {
    pub fn of_endpoint(address: u8) -> Self {
        if address & 0x80 != 0 {
            Self::In
        } else {
            Self::Out
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Unknown,
    Low,
    Full,
    High,
    Wireless,
    Super,
    SuperPlus,
}

impl Speed {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Low,
            2 => Self::Full,
            3 => Self::High,
            4 => Self::Wireless,
            5 => Self::Super,
            6 => Self::SuperPlus,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedDevice {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: Speed,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub num_interfaces: u8,
    pub interfaces: Vec<Interface>,
}

impl ExportedDevice {
    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | (self.devnum & 0xffff)
    }

    pub fn decode(buf: &[u8; DEVICE_SIZE]) -> Self {
        let mut b = &buf[PATH_SIZE + BUSID_SIZE..];
        Self {
            path: c_string(&buf[..PATH_SIZE]),
            busid: c_string(&buf[PATH_SIZE..PATH_SIZE + BUSID_SIZE]),
            busnum: b.get_u32(),
            devnum: b.get_u32(),
            speed: Speed::from_u32(b.get_u32()),
            id_vendor: b.get_u16(),
            id_product: b.get_u16(),
            bcd_device: b.get_u16(),
            device_class: b.get_u8(),
            device_subclass: b.get_u8(),
            device_protocol: b.get_u8(),
            configuration_value: b.get_u8(),
            num_configurations: b.get_u8(),
            num_interfaces: b.get_u8(),
            interfaces: Vec::new(),
        }
    }
}

fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

pub fn encode_busid(busid: &str) -> Result<[u8; BUSID_SIZE]> {
    let bytes = busid.as_bytes();
    if bytes.len() >= BUSID_SIZE || bytes.contains(&0) {
        return Err(ProtocolError::Invalid(format!("invalid busid {busid:?}")));
    }
    let mut out = [0u8; BUSID_SIZE];
    out[..bytes.len()].copy_from_slice(bytes);
    Ok(out)
}

pub fn encode_devlist_request() -> [u8; OP_HEADER_SIZE] {
    let mut buf = [0u8; OP_HEADER_SIZE];
    let mut b = &mut buf[..];
    b.put_u16(USBIP_VERSION);
    b.put_u16(OP_REQ_DEVLIST);
    b.put_u32(0);
    buf
}

pub fn encode_import_request(busid: &str) -> Result<[u8; OP_HEADER_SIZE + BUSID_SIZE]> {
    let mut buf = [0u8; OP_HEADER_SIZE + BUSID_SIZE];
    let mut b = &mut buf[..];
    b.put_u16(USBIP_VERSION);
    b.put_u16(OP_REQ_IMPORT);
    b.put_u32(0);
    b.put_slice(&encode_busid(busid)?);
    Ok(buf)
}

async fn read_op_header<R: AsyncRead + Unpin>(r: &mut R, expected: u16) -> Result<()> {
    let mut buf = [0u8; OP_HEADER_SIZE];
    r.read_exact(&mut buf).await?;
    let mut b = &buf[..];
    let _version = b.get_u16();
    let code = b.get_u16();
    let status = b.get_u32();
    if code != expected {
        return Err(ProtocolError::UnexpectedReply {
            expected,
            actual: code,
        });
    }
    if status != 0 {
        return Err(ProtocolError::Status(status));
    }
    Ok(())
}

async fn read_device<R: AsyncRead + Unpin>(r: &mut R) -> Result<ExportedDevice> {
    let mut buf = [0u8; DEVICE_SIZE];
    r.read_exact(&mut buf).await?;
    Ok(ExportedDevice::decode(&buf))
}

pub async fn read_devlist_reply<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<ExportedDevice>> {
    read_op_header(r, OP_REP_DEVLIST).await?;
    let count = r.read_u32().await?;
    if count > MAX_EXPORTED_DEVICES {
        return Err(ProtocolError::Invalid(format!("device count {count}")));
    }
    let mut devices = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut device = read_device(r).await?;
        let mut raw = vec![0u8; device.num_interfaces as usize * INTERFACE_SIZE];
        r.read_exact(&mut raw).await?;
        device.interfaces = raw
            .as_chunks::<INTERFACE_SIZE>()
            .0
            .iter()
            .map(|c| Interface {
                class: c[0],
                subclass: c[1],
                protocol: c[2],
            })
            .collect();
        devices.push(device);
    }
    Ok(devices)
}

pub async fn read_import_reply<R: AsyncRead + Unpin>(r: &mut R) -> Result<ExportedDevice> {
    read_op_header(r, OP_REP_IMPORT).await?;
    read_device(r).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoPacketDescriptor {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

impl IsoPacketDescriptor {
    fn encode(&self, buf: &mut BytesMut) {
        buf.put_u32(self.offset);
        buf.put_u32(self.length);
        buf.put_u32(self.actual_length);
        buf.put_i32(self.status);
    }

    fn decode(mut b: &[u8]) -> Self {
        Self {
            offset: b.get_u32(),
            length: b.get_u32(),
            actual_length: b.get_u32(),
            status: b.get_i32(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdSubmit {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: u32,
    pub interval: u32,
    pub setup: [u8; 8],
    /// Only sent for OUT transfers.
    pub data: Vec<u8>,
    /// Non-empty only for isochronous transfers.
    pub iso_packets: Vec<IsoPacketDescriptor>,
}

impl CmdSubmit {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.reserve(
            URB_HEADER_SIZE + self.data.len() + self.iso_packets.len() * ISO_DESCRIPTOR_SIZE,
        );
        buf.put_u32(USBIP_CMD_SUBMIT);
        buf.put_u32(self.seqnum);
        buf.put_u32(self.devid);
        buf.put_u32(self.direction);
        buf.put_u32(self.ep);
        buf.put_u32(self.transfer_flags);
        buf.put_u32(self.transfer_buffer_length);
        buf.put_u32(self.start_frame);
        // Linux treats this as the packet count only for isochronous pipes.
        buf.put_u32(if self.iso_packets.is_empty() {
            0
        } else {
            self.iso_packets.len() as u32
        });
        buf.put_u32(self.interval);
        buf.put_slice(&self.setup);
        buf.put_slice(&self.data);
        for p in &self.iso_packets {
            p.encode(buf);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdUnlink {
    pub seqnum: u32,
    pub devid: u32,
    pub unlink_seqnum: u32,
}

impl CmdUnlink {
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.reserve(URB_HEADER_SIZE);
        buf.put_u32(USBIP_CMD_UNLINK);
        buf.put_u32(self.seqnum);
        buf.put_u32(self.devid);
        buf.put_u32(0);
        buf.put_u32(0);
        buf.put_u32(self.unlink_seqnum);
        buf.put_bytes(0, 24);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetSubmit {
    pub seqnum: u32,
    pub status: i32,
    pub actual_length: u32,
    pub start_frame: u32,
    pub error_count: u32,
    pub data: Vec<u8>,
    pub iso_packets: Vec<IsoPacketDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetUnlink {
    pub seqnum: u32,
    pub status: i32,
}

/// A reply header whose payload has not been read yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Submit {
        seqnum: u32,
        status: i32,
        actual_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        error_count: u32,
    },
    Unlink(RetUnlink),
}

pub async fn read_reply_header<R: AsyncRead + Unpin>(r: &mut R) -> Result<Reply> {
    let mut buf = [0u8; URB_HEADER_SIZE];
    r.read_exact(&mut buf).await?;
    let mut b = &buf[..];
    let command = b.get_u32();
    let seqnum = b.get_u32();
    b.advance(12);
    match command {
        USBIP_RET_SUBMIT => {
            let status = b.get_i32();
            let actual_length = b.get_u32();
            let start_frame = b.get_u32();
            let number_of_packets = match b.get_u32() {
                0xffff_ffff => 0,
                n => n,
            };
            let error_count = b.get_u32();
            if number_of_packets > MAX_ISO_PACKETS {
                return Err(ProtocolError::Invalid(format!(
                    "number_of_packets {number_of_packets}"
                )));
            }
            Ok(Reply::Submit {
                seqnum,
                status,
                actual_length,
                start_frame,
                number_of_packets,
                error_count,
            })
        }
        USBIP_RET_UNLINK => Ok(Reply::Unlink(RetUnlink {
            seqnum,
            status: b.get_i32(),
        })),
        other => Err(ProtocolError::UnexpectedCommand(other)),
    }
}

/// Reads the payload of a RET_SUBMIT. `data_expected` is the transfer direction being IN;
/// `max_length` is what we asked for and bounds what the server may send back.
pub async fn read_submit_payload<R: AsyncRead + Unpin>(
    r: &mut R,
    header: &Reply,
    data_expected: bool,
    max_length: u32,
) -> Result<RetSubmit> {
    let Reply::Submit {
        seqnum,
        status,
        actual_length,
        start_frame,
        number_of_packets,
        error_count,
    } = *header
    else {
        return Err(ProtocolError::Invalid("not a RET_SUBMIT".into()));
    };
    if actual_length > max_length {
        return Err(ProtocolError::Invalid(format!(
            "seqnum {seqnum}: actual_length {actual_length} exceeds requested {max_length}"
        )));
    }
    let mut data = Vec::new();
    if data_expected && actual_length > 0 {
        data.resize(actual_length as usize, 0);
        r.read_exact(&mut data).await?;
    }
    let mut iso_packets = Vec::with_capacity(number_of_packets as usize);
    if number_of_packets > 0 {
        let mut raw = vec![0u8; number_of_packets as usize * ISO_DESCRIPTOR_SIZE];
        r.read_exact(&mut raw).await?;
        iso_packets.extend(
            raw.as_chunks::<ISO_DESCRIPTOR_SIZE>()
                .0
                .iter()
                .map(|c| IsoPacketDescriptor::decode(c)),
        );
    }
    Ok(RetSubmit {
        seqnum,
        status,
        actual_length,
        start_frame,
        error_count,
        data,
        iso_packets,
    })
}

/// Linux packs isochronous IN data back to back; spread it out to the descriptor offsets.
pub fn unpack_iso_in(
    packed: &[u8],
    packets: &[IsoPacketDescriptor],
    buffer_length: usize,
) -> Option<Vec<u8>> {
    let mut out = vec![0u8; buffer_length];
    let mut cursor = 0usize;
    for p in packets {
        let len = p.actual_length as usize;
        let start = p.offset as usize;
        let src = packed.get(cursor..cursor + len)?;
        out.get_mut(start..start + len)?.copy_from_slice(src);
        cursor += len;
    }
    Some(out)
}
