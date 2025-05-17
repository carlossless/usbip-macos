use std::{ffi::CStr, fmt::Debug, io::Error};

use bytes::{Buf, BufMut, BytesMut};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpStream, stream};

const USBIP_VERSION: u16 = 273;

struct Interface {
    interface_class: u8,
    interface_subclass: u8,
    interface_protocol: u8,
}

struct Device {
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

#[repr(u16)]
enum UsbIpCommand {
    ReqDevlist = 0x8005,
    ReqImport = 0x8003,
}

impl Debug for UsbIpCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsbIpCommand::ReqDevlist => f.write_str("USBIP_REQ_DEVLIST"),
            UsbIpCommand::ReqImport => f.write_str("USBIP_REQ_IMPORT"),
        }
    }
}

impl Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("path", unsafe { &CStr::from_ptr(self.path.as_ptr() as *const i8) })
            .field("busid", unsafe { &CStr::from_ptr(self.busid.as_ptr() as *const i8) })
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

struct UsbIpClient {
    stream: Option<TcpStream>
}

impl UsbIpClient {
    pub fn new() -> Self {
        UsbIpClient { stream: None }
    }

    pub async fn connect(&mut self, addr: &str) -> Result<(), Error> {
        let stream = TcpStream::connect(addr).await?;
        self.stream = Some(stream);
        Ok(())
    }

    pub async fn list_devices(&mut self) -> Result<Vec<Device>, Error> {
        if let None = self.stream {
            return Err(Error::new(std::io::ErrorKind::NotConnected, "Not connected to USBIP server"));
        }

        println!("Sending {:?} Request", UsbIpCommand::ReqDevlist);

        let stream = self.stream.as_mut().unwrap();

        let mut request = BytesMut::with_capacity(48);
        request.put_u16(USBIP_VERSION);    // version
        request.put_u16(UsbIpCommand::ReqDevlist as u16); // command code
        request.put_u32(0);      // status
        request.resize(8, 0);   // pad to header size
        stream.write_all(&request).await?;

        stream.read_u16().await?;
        stream.read_u16().await?;
        stream.read_u32().await?;
        let device_count = stream.read_u32().await?;

        println!("Received USBIP_DEVLIST response, device count: {}", device_count);

        let mut devices: Vec<Device> = Vec::with_capacity(device_count as usize);
        for d in 0..device_count {
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
            return Err(Error::new(std::io::ErrorKind::NotConnected, "Not connected to USBIP server"));
        }

        println!("Sending {:?} Request", UsbIpCommand::ReqImport);

        let stream = self.stream.as_mut().unwrap();

        let mut request = BytesMut::with_capacity(48);
        request.put_u16(USBIP_VERSION);    // version
        request.put_u16(UsbIpCommand::ReqImport as u16); // command code
        request.put_u32(0);      // status
        request.put(&busid[..]);
        request.resize(40, 0);   // pad to header size
        stream.write_all(&request).await?;


        stream.read_u16().await?;
        stream.read_u16().await?;
        let status = stream.read_u32().await?;
        if status != 0 {
            return Err(Error::new(std::io::ErrorKind::Other, "Failed to import device"));
        }

        println!("Received {:?} response, status: {}", UsbIpCommand::ReqImport, status);

        Ok(())
    }

    pub async fn disconnect(&mut self) {
        self.stream.as_mut().unwrap().shutdown().await.unwrap();
        self.stream = None;
    }

}

pub async fn connect_to_usbip_server(addr: &str) -> Result<(), Error> {
    let mut client = UsbIpClient::new();
    client.connect(addr).await?;

    let devices: Vec<Device> = client.list_devices().await?;
    println!("{:?}", devices);

    // client.disconnect().await; will be disconnected from server anyway

    client.connect(addr).await?;

    let device = devices.first().unwrap();

    client.import_device(device.busid).await?;

    Ok(())
}
