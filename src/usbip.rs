use std::{collections::HashMap, ffi::CStr, fmt::Debug, io::Error, sync::Arc};

use bytes::{Buf, BufMut, BytesMut};
use log::{debug, error};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{oneshot, Mutex},
};

const USBIP_VERSION: u16 = 0x0111; // 1.1.1

type UsbIpSeqnum = u32;

pub struct Interface {
    interface_class: u8,
    interface_subclass: u8,
    interface_protocol: u8,
}

pub struct Device {
    path: [u8; 256],
    busid: [u8; 32],
    busnum: u32,
    devnum: u32,
    speed: u32,
    id_vendor: u16,
    id_product: u16,
    bcd_device: u16,
    device_class: u8,
    device_subclass: u8,
    device_protocol: u8,
    configuration_value: u8,
    num_configurations: u8,
    num_interfaces: u8,
    interfaces: Vec<Interface>,
}

impl Device {
    pub fn get_busid(&self) -> &[u8; 32] {
        &self.busid
    }

    pub fn get_devnum(&self) -> u32 {
        self.devnum
    }

    pub fn get_id_vendor(&self) -> u16 {
        self.id_vendor
    }

    pub fn get_id_product(&self) -> u16 {
        self.id_product
    }
}

#[repr(u16)]
enum UsbIpCommand {
    ReqDevlist = 0x8005,
    ReqImport = 0x8003,
}

impl Debug for UsbIpCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReqDevlist => f.write_str("USBIP_REQ_DEVLIST"),
            Self::ReqImport => f.write_str("USBIP_REQ_IMPORT"),
        }
    }
}

#[repr(u32)]
enum UsbIpCommand2 {
    CmdSubmit = 0x00000001,
}

impl Debug for UsbIpCommand2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CmdSubmit => f.write_str("USBIP_CMD_SUBMIT"),
        }
    }
}

#[repr(u32)]
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum UsbIpDirection {
    UsbDirOut = 0,
    UsbDirIn = 1,
}

pub struct UsbCommandHeader {
    command: u32,
    seqnum: u32,
    devid: u32,
    direction: u32,
    ep_number: u32,
}

impl Debug for UsbCommandHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbCommandHeader")
            .field("command", &self.command)
            .field("seqnum", &self.seqnum)
            .field("devid", &self.devid)
            .field("direction", &self.direction)
            .field("ep_number", &self.ep_number)
            .finish()
    }
}

impl UsbCommandHeader {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut buf = bytes;
        let command = buf.get_u32();
        let seqnum = buf.get_u32();
        let devid = buf.get_u32();
        let direction = buf.get_u32();
        let ep_number = buf.get_u32();

        UsbCommandHeader {
            command,
            seqnum,
            devid,
            direction,
            ep_number,
        }
    }

    pub async fn from_stream(stream: &mut TcpStream) -> Result<Self, Error> {
        let mut buf = [0u8; 20];
        stream.read_exact(&mut buf).await?;
        Ok(UsbCommandHeader::from_bytes(&buf))
    }

    pub fn to_bytes(&self) -> [u8; 20] {
        let mut buf = Vec::new();
        buf.put_u32(self.command);
        buf.put_u32(self.seqnum);
        buf.put_u32(self.devid);
        buf.put_u32(self.direction);
        buf.put_u32(self.ep_number);
        buf.try_into().unwrap()
    }
}

pub struct UsbCommandSubmit {
    header: UsbCommandHeader,
    transfer_flags: u32,
    transfer_buffer_length: u32,
    start_frame: u32,
    number_of_packets: u32,
    interval: u32,
    setup_bytes: [u8; 8],
    transfer_buffer: Vec<u8>,
}

impl Debug for UsbCommandSubmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbCommandSubmit")
            .field("header", &self.header)
            .field("transfer_flags", &self.transfer_flags)
            .field("transfer_buffer_length", &self.transfer_buffer_length)
            .field("start_frame", &self.start_frame)
            .field("number_of_packets", &self.number_of_packets)
            .field("interval", &self.interval)
            .field("setup_bytes", &self.setup_bytes)
            .field(
                "transfer_buffer",
                &format_args!("{:#04x?}", self.transfer_buffer),
            )
            .finish()
    }
}

impl UsbCommandSubmit {
    pub fn new(
        header: UsbCommandHeader,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup_bytes: [u8; 8],
        transfer_buffer: Vec<u8>,
    ) -> Self {
        UsbCommandSubmit {
            header,
            transfer_flags,
            transfer_buffer_length,
            start_frame,
            number_of_packets,
            interval,
            setup_bytes,
            transfer_buffer,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.put(&self.header.to_bytes()[..]);
        buf.put_u32(self.transfer_flags);
        buf.put_u32(self.transfer_buffer_length);
        buf.put_u32(self.start_frame);
        buf.put_u32(self.number_of_packets);
        buf.put_u32(self.interval);
        buf.put(&self.setup_bytes[..]);
        buf.put(&self.transfer_buffer[..]);
        buf
    }
}

pub struct UsbReturnSubmit {
    header: UsbCommandHeader,
    status: u32,
    pub actual_length: u32,
    start_frame: u32,
    number_of_packets: u32,
    error_count: u32,
    pub buffer: Vec<u8>,
}

impl UsbReturnSubmit {
    pub async fn from_header_and_stream(
        direction: UsbIpDirection,
        header: UsbCommandHeader,
        stream: &mut TcpStream,
    ) -> Result<Self, Error> {
        let status = stream.read_u32().await?;
        let actual_length = stream.read_u32().await?;
        let start_frame = stream.read_u32().await?;
        let number_of_packets = stream.read_u32().await?;
        let error_count = stream.read_u32().await?;
        stream.read_u64().await?; // skip padding
        let buffer = if direction == UsbIpDirection::UsbDirIn {
            let mut buf = vec![0; actual_length as usize];
            stream.read_exact(&mut buf).await?;
            buf
        } else {
            vec![]
        };

        Ok(UsbReturnSubmit {
            header,
            status,
            actual_length,
            start_frame,
            number_of_packets,
            error_count,
            buffer,
        })
    }
}

impl Debug for UsbReturnSubmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbReturnSubmit")
            .field("header", &self.header)
            .field("status", &self.status)
            .field("actual_length", &self.actual_length)
            .field("start_frame", &self.start_frame)
            .field("number_of_packets", &self.number_of_packets)
            .field("error_count", &self.error_count)
            .field("buffer", &format_args!("{:#04x?}", self.buffer))
            .finish()
    }
}

impl Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("path", unsafe {
                &CStr::from_ptr(self.path.as_ptr() as *const i8)
            })
            .field("busid", unsafe {
                &CStr::from_ptr(self.busid.as_ptr() as *const i8)
            })
            .field("busnum", &self.busnum)
            .field("devnum", &self.devnum)
            .field("speed", &self.speed)
            .field("id_vendor", &format_args!("{:#06x}", self.id_vendor))
            .field("id_product", &format_args!("{:#06x}", self.id_product))
            .field("bcd_device", &self.bcd_device)
            .field("device_class", &self.device_class)
            .field("device_subclass", &self.device_subclass)
            .field("device_protocol", &self.device_protocol)
            .field("configuration_value", &self.configuration_value)
            .field("num_configurations", &self.num_configurations)
            .field("num_interfaces", &self.num_interfaces)
            .field("interfaces", &self.interfaces)
            .finish()
    }
}

impl Debug for Interface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interface")
            .field("interface_class", &self.interface_class)
            .field("interface_subclass", &self.interface_subclass)
            .field("interface_protocol", &self.interface_protocol)
            .finish()
    }
}

pub struct UsbIpClient {
    stream: Option<TcpStream>,
    imported_device: Option<Device>,
    seqnum: u32,
    pending: Arc<Mutex<HashMap<UsbIpSeqnum, (UsbIpDirection, oneshot::Sender<UsbReturnSubmit>)>>>,
}

unsafe impl Send for UsbIpClient {}
unsafe impl Sync for UsbIpClient {}

impl UsbIpClient {
    pub fn new() -> Self {
        UsbIpClient {
            stream: None,
            imported_device: None,
            seqnum: 0,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn connect(&mut self, addr: &str) -> Result<(), Error> {
        let stream = TcpStream::connect(addr).await?;
        self.stream = Some(stream);
        Ok(())
    }

    pub async fn list_devices(&mut self) -> Result<Vec<Device>, Error> {
        if let None = self.stream {
            // TODO: fix
            return Err(Error::new(
                std::io::ErrorKind::NotConnected,
                "Not connected to USBIP server",
            ));
        }

        debug!("Sending {:?} Request", UsbIpCommand::ReqDevlist);

        let stream = self.stream.as_mut().unwrap();

        let mut request = BytesMut::with_capacity(48);
        request.put_u16(USBIP_VERSION); // version
        request.put_u16(UsbIpCommand::ReqDevlist as u16); // command code
        request.put_u32(0); // status
        request.resize(8, 0); // pad to header size
        stream.write_all(&request).await?;

        stream.read_u16().await?;
        stream.read_u16().await?;
        stream.read_u32().await?;
        let device_count = stream.read_u32().await?;

        debug!(
            "Received USBIP_DEVLIST response, device count: {}",
            device_count
        );

        let mut devices: Vec<Device> = Vec::with_capacity(device_count as usize);
        for _d in 0..device_count {
            let mut path: [u8; 256] = [0; 256];
            let mut busid: [u8; 32] = [0; 32];
            stream.read_exact(&mut path).await?;
            stream.read_exact(&mut busid).await?;

            let busnum = stream.read_u32().await?;
            let devnum = stream.read_u32().await?;
            let speed = stream.read_u32().await?;
            let id_vendor = stream.read_u16().await?;
            let id_product = stream.read_u16().await?;
            let bcd_device = stream.read_u16().await?;
            let device_class = stream.read_u8().await?;
            let device_subclass = stream.read_u8().await?;
            let device_protocol = stream.read_u8().await?;
            let configuration_value = stream.read_u8().await?;
            let num_configurations = stream.read_u8().await?;
            let num_interfaces = stream.read_u8().await?;
            let mut interfaces: Vec<Interface> = Vec::with_capacity(num_interfaces as usize);
            for _ in 0..(num_interfaces as usize) {
                let interface_class = stream.read_u8().await?;
                let interface_subclass = stream.read_u8().await?;
                let interface_protocol = stream.read_u8().await?;
                stream.read_u8().await?; // alignment padding
                interfaces.push(Interface {
                    interface_class,
                    interface_subclass,
                    interface_protocol,
                });
            }

            let device = Device {
                path,
                busid,
                busnum,
                devnum,
                speed,
                id_vendor,
                id_product,
                bcd_device,
                device_class,
                device_subclass,
                device_protocol,
                configuration_value,
                num_configurations,
                num_interfaces,
                interfaces,
            };
            devices.push(device);
        }

        Ok(devices)
    }

    pub async fn import_device(&mut self, busid: [u8; 32]) -> Result<(), Error> {
        if let None = self.stream {
            return Err(Error::new(
                std::io::ErrorKind::NotConnected,
                "Not connected to USBIP server",
            ));
        }

        debug!("Sending {:?} Request", UsbIpCommand::ReqImport);

        let stream = self.stream.as_mut().unwrap();

        let mut request = BytesMut::with_capacity(48);
        request.put_u16(USBIP_VERSION); // version
        request.put_u16(UsbIpCommand::ReqImport as u16); // command code
        request.put_u32(0); // status
        request.put(&busid[..]);
        request.resize(40, 0); // pad to header size
        stream.write_all(&request).await?;

        stream.read_u16().await?; // version
        stream.read_u16().await?; // reply code
        let status = stream.read_u32().await?;
        if status != 0 {
            return Err(Error::new(
                std::io::ErrorKind::Other,
                "Failed to import device",
            ));
        }

        let mut path: [u8; 256] = [0; 256];
        let mut busid: [u8; 32] = [0; 32];
        stream.read_exact(&mut path).await?;
        stream.read_exact(&mut busid).await?;
        let busnum = stream.read_u32().await?;
        let devnum = stream.read_u32().await?;
        let speed = stream.read_u32().await?;
        let id_vendor = stream.read_u16().await?;
        let id_product = stream.read_u16().await?;
        let bcd_device = stream.read_u16().await?;
        let device_class = stream.read_u8().await?;
        let device_subclass = stream.read_u8().await?;
        let device_protocol = stream.read_u8().await?;
        let configuration_value = stream.read_u8().await?;
        let num_configurations = stream.read_u8().await?;
        let num_interfaces = stream.read_u8().await?;
        let interfaces: Vec<Interface> = vec![];

        let device = Device {
            path,
            busid,
            busnum,
            devnum,
            speed,
            id_vendor,
            id_product,
            bcd_device,
            device_class,
            device_subclass,
            device_protocol,
            configuration_value,
            num_configurations,
            num_interfaces,
            interfaces,
        };

        debug!(
            "Received {:?} response, status: {}, {:?}",
            UsbIpCommand::ReqImport,
            status,
            &device
        );

        self.imported_device = Some(device);

        Ok(())
    }

    pub async fn cmd_submit(
        &mut self,
        direction: UsbIpDirection,
        ep: u32,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup_bytes: [u8; 8],
        transfer_buffer: &[u8],
    ) -> Result<UsbReturnSubmit, Error> {
        let Some(stream) = &mut self.stream else {
            return Err(Error::new(
                std::io::ErrorKind::NotConnected,
                "Not connected to USBIP server",
            ));
        };

        let Some(device) = &self.imported_device else {
            return Err(Error::new(
                std::io::ErrorKind::NotConnected,
                "Device not imported",
            ));
        };

        self.seqnum = self.seqnum.wrapping_add(1);

        let mut request = BytesMut::new();

        let cmd = UsbCommandSubmit::new(
            UsbCommandHeader {
                command: UsbIpCommand2::CmdSubmit as u32,
                seqnum: self.seqnum,
                devid: (device.busnum << 16) | device.devnum,
                direction: direction as u32,
                ep_number: ep,
            },
            transfer_flags,
            transfer_buffer_length,
            start_frame,
            number_of_packets,
            interval,
            setup_bytes,
            transfer_buffer.to_vec(),
        );

        request.put(&cmd.to_bytes()[..]);

        stream.write_all(&request).await?;
        debug!("Sent USBIP_CMD_SUBMIT request {:?}", cmd);

        let (tx, rx) = oneshot::channel();

        // Register the pending request
        self.pending
            .lock()
            .await
            .insert(cmd.header.seqnum.clone(), (direction, tx));

        // Wait for the matching response
        let response = rx.await.unwrap();

        Ok(response)
    }

    pub async fn poll(&mut self) -> Result<(), Error> {
        let Some(stream) = &mut self.stream else {
            return Err(Error::new(
                std::io::ErrorKind::NotConnected,
                "Not connected to USBIP server",
            ));
        };

        stream.readable().await?;

        let header = UsbCommandHeader::from_stream(stream).await?;
        let secnum = header.seqnum;
        if let Some((direction, tx)) = self.pending.lock().await.remove(&secnum) {
            let ret = UsbReturnSubmit::from_header_and_stream(direction, header, stream).await?;
            let _ = tx.send(ret);
        } else {
            error!("Received response with unknown secnum: {}", secnum);
        }
        return Ok(());
    }
}
