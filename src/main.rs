use std::{io::Error};

use controller_interface::ControllerInterface;
use dispatch2::dispatch_main;
use tokio;
use usbip::connect_to_usbip_server;

mod controller_interface;
mod device;
mod endpoint;

mod usbip;

#[repr(u8)]
enum UsbDescriptor {
    USB_DESC_DEVICE                    = 1,
    USB_DESC_CONFIGURATION             = 2,
    USB_DESC_STRING                    = 3,
    USB_DESC_INTERFACE                 = 4,
    USB_DESC_ENDPOINT                  = 5,
    USB_DESC_DEVICE_QUALIFIER          = 6,
    USB_DESC_OTHER_SPEED_CONFIGURATION = 7,
    USB_DESC_INTERFACE_POWER           = 8,

    USB_DESC_CLASS_HID    = 0x21,
    USB_DESC_CLASS_REPORT = 0x22,
}

#[repr(u8)]
enum UsbClass {
    USB_DEV_CLASS_PER_INTERFACE    = 0x00,
    // USB_DEV_SUBCLASS_PER_INTERFACE = 0x00,
    // USB_DEV_PROTOCOL_PER_INTERFACE = 0x00,

    USB_DEV_CLASS_VENDOR    = 0xff,
    // USB_DEV_SUBCLASS_VENDOR = 0xff,
    // USB_DEV_PROTOCOL_VENDOR = 0xff,
}

#[repr(u8)]
enum UsbIface {
    USB_IFACE_CLASS_APP_SPECIFIC = 0xfe,

    USB_IFACE_CLASS_HID          = 0x03,
    USB_IFACE_CLASS_VENDOR       = 0xff,
    
    USB_IFACE_SUBCLASS_NONE     = 0x00,
    USB_IFACE_SUBCLASS_HID_BOOT = 0x01,
    // USB_IFACE_SUBCLASS_VENDOR   = 0xff,

    // USB_IFACE_PROTOCOL_BOOT     = 0x00,
    // USB_IFACE_PROTOCOL_REPORT   = 0x01,
    // USB_IFACE_PROTOCOL_VENDOR   = 0xff,
}    

#[repr(packed)]
struct UsbDescDevice {
    bLength: u8,
    bDescriptorType: u8,
    bcdUSB: u16,
    bDeviceClass: u8,
    bDeviceSubClass: u8,
    bDeviceProtocol: u8,
    bMaxPacketSize0: u8,
    idVendor: u16,
    idProduct: u16,
    bcdDevice: u16,
    iManufacturer: u8,
    iProduct: u8,
    iSerialNumber: u8,
    bNumConfigurations: u8,
}

#[repr(packed)]
struct UsbDescConfiguration {
    bLength: u8,
    bDescriptorType: u8,
    wTotalLength: u16,
    bNumInterfaces: u8,
    bConfigurationValue: u8,
    iConfiguration: u8,
    bmAttributes: u8,
    bMaxPower: u8,
}

#[repr(packed)]
struct UsbDescInterface {
    bLength: u8,
    bDescriptorType: u8,
    bInterfaceNumber: u8,
    bAlternateSetting: u8,
    bNumEndpoints: u8,
    bInterfaceClass: u8,
    bInterfaceSubClass: u8,
    bInterfaceProtocol: u8,
    iInterface: u8,
}

#[repr(packed)]
struct UsbDesc {
    usb_desc: UsbDescConfiguration,
    usb_desc_interface: UsbDescInterface,
}


#[repr(u8)]
enum UsbDirection {
    USB_DIR_OUT = 0b00000000,
    USB_DIR_IN  = 0b10000000,

    // USB_DIR_MASK = 0b10000000,
}

#[repr(u8)]
enum UsbType {
    USB_TYPE_STANDARD = 0b00000000,
    USB_TYPE_CLASS    = 0b00100000,
    USB_TYPE_VENDOR   = 0b01000000,

    USB_TYPE_MASK = 0b01100000,
}

#[repr(u8)]
enum UsbRecipient {
    USB_RECIP_DEVICE = 0b00000000,
    USB_RECIP_IFACE  = 0b00000001,
    USB_RECIP_ENDPT  = 0b00000010,
    USB_RECIP_OTHER  = 0b00000011,

    USB_RECIP_MASK = 0b00001111,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    connect_to_usbip_server("carlossless-chedar:3240").await?;

    let con_iface = ControllerInterface::new();

    dispatch_main();

    connect_to_usbip_server("carlossless-chedar:3240").await?;

    Ok(())
}
