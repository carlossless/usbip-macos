use std::{
    collections::HashMap, ffi::CStr, fmt::Debug, io::Error, sync::Arc,
};

use anyhow::anyhow;
use bytes::{Buf, BufMut, BytesMut};
use log::{debug, error};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{oneshot, Mutex},
};

const USBIP_VERSION: u16 = 0x0111;
const SETUP_BYTES_SIZE: usize = 8;
const USB_COMMAND_HEADER_SIZE: usize = 20;

type UsbIpSeqnum = u32;
type UsbIpResult<T> = Result<T, anyhow::Error>;

#[derive(Debug)]
pub struct Interface {
    interface_class: u8,
    interface_subclass: u8,
    interface_protocol: u8,
}

impl Interface {
    pub fn new(interface_class: u8, interface_subclass: u8, interface_protocol: u8) -> Self {
        Self {
            interface_class,
            interface_subclass,
            interface_protocol,
        }
    }
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
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
    ) -> Self {
        Self {
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
        }
    }

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

#[repr(u16)]
#[derive(Debug, Clone, Copy)]
enum UsbIpCommand {
    ReqDevlist = 0x8005,
    ReqImport = 0x8003,
}

impl UsbIpCommand {
    fn name(self) -> &'static str {
        match self {
            Self::ReqDevlist => "USBIP_REQ_DEVLIST",
            Self::ReqImport => "USBIP_REQ_IMPORT",
        }
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy)]
enum UsbIpCmdType {
    CmdSubmit = 0x00000001,
}

impl UsbIpCmdType {
    fn name(self) -> &'static str {
        match self {
            Self::CmdSubmit => "USBIP_CMD_SUBMIT",
        }
    }
}

#[repr(u32)]
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum UsbIpDirection {
    UsbDirOut = 0,
    UsbDirIn = 1,
}

#[derive(Debug, Clone)]
pub struct UsbCommandHeader {
    command: u32,
    seqnum: u32,
    devid: u32,
    direction: u32,
    ep_number: u32,
}

impl UsbCommandHeader {
    pub fn new(command: u32, seqnum: u32, devid: u32, direction: u32, ep_number: u32) -> Self {
        Self {
            command,
            seqnum,
            devid,
            direction,
            ep_number,
        }
    }

    pub fn seqnum(&self) -> u32 {
        self.seqnum
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut buf = bytes;
        Self::new(
            buf.get_u32(),
            buf.get_u32(),
            buf.get_u32(),
            buf.get_u32(),
            buf.get_u32(),
        )
    }

    pub async fn from_stream(stream: &mut TcpStream) -> UsbIpResult<Self> {
        let mut buf = [0u8; USB_COMMAND_HEADER_SIZE];
        stream.read_exact(&mut buf).await?;
        Ok(Self::from_bytes(&buf))
    }

    pub fn to_bytes(&self) -> [u8; USB_COMMAND_HEADER_SIZE] {
        let mut buf = Vec::new();
        buf.put_u32(self.command);
        buf.put_u32(self.seqnum);
        buf.put_u32(self.devid);
        buf.put_u32(self.direction);
        buf.put_u32(self.ep_number);
        buf.try_into().unwrap()
    }
}

#[derive(Debug, Clone)]
pub struct UsbCommandSubmit {
    header: UsbCommandHeader,
    transfer_flags: u32,
    transfer_buffer_length: u32,
    start_frame: u32,
    number_of_packets: u32,
    interval: u32,
    setup_bytes: [u8; SETUP_BYTES_SIZE],
    transfer_buffer: Vec<u8>,
}

impl UsbCommandSubmit {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        header: UsbCommandHeader,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup_bytes: [u8; SETUP_BYTES_SIZE],
        transfer_buffer: Vec<u8>,
    ) -> Self {
        Self {
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

#[derive(Debug)]
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
    ) -> UsbIpResult<Self> {
        let status = stream.read_u32().await?;
        let actual_length = stream.read_u32().await?;
        let start_frame = stream.read_u32().await?;
        let number_of_packets = stream.read_u32().await?;
        let error_count = stream.read_u32().await?;
        stream.read_u64().await?;

        let buffer = if direction == UsbIpDirection::UsbDirIn {
            let mut buf = vec![0; actual_length as usize];
            stream.read_exact(&mut buf).await?;
            buf
        } else {
            vec![]
        };

        Ok(Self {
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
        Self {
            stream: None,
            imported_device: None,
            seqnum: 0,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn connect(&mut self, addr: &str) -> UsbIpResult<()> {
        let stream = TcpStream::connect(addr).await?;
        self.stream = Some(stream);
        Ok(())
    }

    pub async fn list_devices(&mut self) -> UsbIpResult<Vec<Device>> {
        let stream = self.get_stream_mut()?;

        debug!("Sending {} Request", UsbIpCommand::ReqDevlist.name());

        Self::send_device_list_request(stream).await?;
        let device_count = Self::read_device_list_response_header(stream).await?;

        debug!(
            "Received USBIP_DEVLIST response, device count: {}",
            device_count
        );

        let mut devices = Vec::with_capacity(device_count as usize);
        for _ in 0..device_count {
            let device = Self::read_device_from_stream(stream).await?;
            devices.push(device);
        }

        Ok(devices)
    }

    pub async fn import_device(&mut self, busid: [u8; 32]) -> UsbIpResult<()> {
        let stream = self.get_stream_mut()?;

        debug!("Sending {} Request", UsbIpCommand::ReqImport.name());

        Self::send_import_request(stream, &busid).await?;
        Self::read_import_response_header(stream).await?;

        let device = Self::read_imported_device_from_stream(stream).await?;

        debug!(
            "Received {} response, device imported successfully: {:?}",
            UsbIpCommand::ReqImport.name(),
            &device
        );

        self.imported_device = Some(device);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn cmd_submit(
        &mut self,
        direction: UsbIpDirection,
        ep: u32,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup_bytes: [u8; SETUP_BYTES_SIZE],
        transfer_buffer: &[u8],
    ) -> UsbIpResult<UsbReturnSubmit> {
        self.seqnum = self.seqnum.wrapping_add(1);
        let seqnum = self.seqnum;

        let cmd = {
            let device = self.get_imported_device()?;
            self.create_submit_command(
                device,
                direction,
                ep,
                transfer_flags,
                transfer_buffer_length,
                start_frame,
                number_of_packets,
                interval,
                setup_bytes,
                transfer_buffer,
            )
        };

        let stream = self.get_stream_mut()?;
        Self::send_submit_command(stream, &cmd).await?;
        debug!("Sent {} request {:?}", UsbIpCmdType::CmdSubmit.name(), cmd);

        let (tx, rx) = oneshot::channel();

        self.pending.lock().await.insert(seqnum, (direction, tx));

        let response = rx
            .await
            .map_err(|_| Error::new(std::io::ErrorKind::Other, "Failed to receive response"))?;

        Ok(response)
    }

    pub async fn poll(&mut self) -> UsbIpResult<()> {
        let stream = match self.stream.as_mut() {
            Some(s) => s,
            None => return Err(anyhow!("Not connected")),
        };

        stream.readable().await?;

        let mut buf = [0u8; 20];
        let bytes_read = stream.peek(&mut buf).await?; // at least a header should be available
        if bytes_read == 0 {
            return Err(anyhow!("Stream is closed"));
        }

        let header = UsbCommandHeader::from_stream(stream).await?;
        let seqnum = header.seqnum();

        let pending_request = self.pending.lock().await.remove(&seqnum);

        if let Some((direction, tx)) = pending_request {
            let response =
                UsbReturnSubmit::from_header_and_stream(direction, header, stream).await?;
            let _ = tx.send(response);
        } else {
            error!("Received response with unknown seqnum: {}", seqnum);
        }

        Ok(())
    }

    fn get_stream_mut(&mut self) -> UsbIpResult<&mut TcpStream> {
        self.stream.as_mut().ok_or_else(|| anyhow!("Not connected"))
    }

    fn get_imported_device(&self) -> UsbIpResult<&Device> {
        self.imported_device
            .as_ref()
            .ok_or_else(|| anyhow!("Device not imported"))
    }

    async fn send_device_list_request(stream: &mut TcpStream) -> UsbIpResult<()> {
        let mut request = BytesMut::with_capacity(48);
        request.put_u16(USBIP_VERSION);
        request.put_u16(UsbIpCommand::ReqDevlist as u16);
        request.put_u32(0);
        request.resize(8, 0);
        stream.write_all(&request).await?;
        Ok(())
    }

    async fn read_device_list_response_header(stream: &mut TcpStream) -> UsbIpResult<u32> {
        stream.read_u16().await?;
        stream.read_u16().await?;
        stream.read_u32().await?;
        let device_count = stream.read_u32().await?;
        Ok(device_count)
    }

    async fn send_import_request(stream: &mut TcpStream, busid: &[u8; 32]) -> UsbIpResult<()> {
        let mut request = BytesMut::with_capacity(48);
        request.put_u16(USBIP_VERSION);
        request.put_u16(UsbIpCommand::ReqImport as u16);
        request.put_u32(0);
        request.put(&busid[..]);
        request.resize(40, 0);
        stream.write_all(&request).await?;
        Ok(())
    }

    async fn read_import_response_header(stream: &mut TcpStream) -> UsbIpResult<()> {
        stream.read_u16().await?;
        stream.read_u16().await?;
        let status = stream.read_u32().await?;
        if status != 0 {
            return Err(anyhow!("Import failed"));
        }
        Ok(())
    }

    async fn read_device_from_stream(stream: &mut TcpStream) -> UsbIpResult<Device> {
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

        let mut interfaces = Vec::with_capacity(num_interfaces as usize);
        for _ in 0..(num_interfaces as usize) {
            let interface_class = stream.read_u8().await?;
            let interface_subclass = stream.read_u8().await?;
            let interface_protocol = stream.read_u8().await?;
            stream.read_u8().await?;
            interfaces.push(Interface::new(
                interface_class,
                interface_subclass,
                interface_protocol,
            ));
        }

        Ok(Device::new(
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
        ))
    }

    async fn read_imported_device_from_stream(stream: &mut TcpStream) -> UsbIpResult<Device> {
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
        let interfaces = vec![];

        Ok(Device::new(
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
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn create_submit_command(
        &self,
        device: &Device,
        direction: UsbIpDirection,
        ep: u32,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup_bytes: [u8; SETUP_BYTES_SIZE],
        transfer_buffer: &[u8],
    ) -> UsbCommandSubmit {
        UsbCommandSubmit::new(
            UsbCommandHeader::new(
                UsbIpCmdType::CmdSubmit as u32,
                self.seqnum,
                (device.busnum << 16) | device.devnum,
                direction as u32,
                ep,
            ),
            transfer_flags,
            transfer_buffer_length,
            start_frame,
            number_of_packets,
            interval,
            setup_bytes,
            transfer_buffer.to_vec(),
        )
    }

    async fn send_submit_command(
        stream: &mut TcpStream,
        cmd: &UsbCommandSubmit,
    ) -> UsbIpResult<()> {
        let mut request = BytesMut::new();
        request.put(&cmd.to_bytes()[..]);
        stream.write_all(&request).await?;
        Ok(())
    }
}
