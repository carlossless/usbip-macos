use core::panic;
use std::{cell::RefCell, ffi::CStr, io::Error, mem, os::raw::c_void, ptr::{from_mut, NonNull}, rc::Rc, slice::{from_raw_parts, from_raw_parts_mut}, sync::Mutex, thread::sleep};

use block2::{global_block, Block, DynBlock, RcBlock};
use dispatch2::dispatch_main;
use objc2::{ffi::NSUIntegerMax, rc::Retained, AnyThread};
use objc2_foundation::{NSDate, NSMutableData, NSRunLoop, NSError};
use tokio;
use usbip::connect_to_usbip_server;
use objc2_io_usb_host::{self, IOUSBHostCICapabilitiesMessageControlPortCountPhase, IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase, IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase, IOUSBHostCICommandMessageData0DeviceAddress, IOUSBHostCICommandMessageData0DeviceAddressPhase, IOUSBHostCICommandMessageData0EndpointAddress, IOUSBHostCICommandMessageData0EndpointAddressPhase, IOUSBHostCIDeviceSpeed, IOUSBHostCIDeviceStateMachine, IOUSBHostCIDoorbell, IOUSBHostCIDoorbellDeviceAddress, IOUSBHostCIDoorbellDeviceAddressPhase, IOUSBHostCIDoorbellEndpointAddress, IOUSBHostCIDoorbellEndpointAddressPhase, IOUSBHostCIDoorbellStreamID, IOUSBHostCIDoorbellStreamIDPhase, IOUSBHostCIEndpointState, IOUSBHostCIEndpointStateMachine, IOUSBHostCILinkState, IOUSBHostCIMessage, IOUSBHostCIMessageControlNoResponse, IOUSBHostCIMessageControlType, IOUSBHostCIMessageControlTypePhase, IOUSBHostCIMessageControlValid, IOUSBHostCIMessageStatus, IOUSBHostCIMessageType, IOUSBHostCINormalTransferData0Length, IOUSBHostCINormalTransferData0LengthPhase, IOUSBHostCINormalTransferData1Buffer, IOUSBHostCINormalTransferData1BufferPhase, IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase, IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase, IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase, IOUSBHostCIPortStatusSpeed, IOUSBHostCISetupTransferData1bRequest, IOUSBHostCISetupTransferData1bRequestPhase, IOUSBHostCISetupTransferData1bmRequestType, IOUSBHostCISetupTransferData1bmRequestTypePhase, IOUSBHostCISetupTransferData1wIndex, IOUSBHostCISetupTransferData1wIndexPhase, IOUSBHostCISetupTransferData1wLength, IOUSBHostCISetupTransferData1wLengthPhase, IOUSBHostCISetupTransferData1wValue, IOUSBHostCISetupTransferData1wValuePhase, IOUSBHostControllerInterface, IOUSBHostControllerInterfaceCommandHandler};

mod usbip;

struct Device {
    device: Retained<IOUSBHostCIDeviceStateMachine>,
    endpoints: Vec<Retained<IOUSBHostCIEndpointStateMachine>>,
}
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

static devices: Mutex<Vec<Device>> = Mutex::new(Vec::new());

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

const device_desc: UsbDescDevice = UsbDescDevice {
    bLength: mem::size_of::<UsbDescDevice>() as u8,
    bDescriptorType: UsbDescriptor::USB_DESC_DEVICE as u8,
    bcdUSB: 0x0110, // USB 1.1
    bDeviceClass: 0x00, // Per interface
    bDeviceSubClass: 0x00, // Per interface
    bDeviceProtocol: 0x00, // Per interface
    bMaxPacketSize0: 8, // Can increase
    idVendor: 0xdead,
    idProduct: 0xbeef,
    bcdDevice: 0x0000,
    iManufacturer: 1,
    iProduct: 2,
    iSerialNumber: 3,
    bNumConfigurations: 1,
};

fn command_handler(
    controller: NonNull<IOUSBHostControllerInterface>,
    command: IOUSBHostCIMessage
) {
    println!("Command handler called with command: {:?}", command);
    let msg_type = IOUSBHostCIMessageType((command.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
    println!("Command type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

    match msg_type {
        IOUSBHostCIMessageType::ControllerPowerOn => {
            println!("I HAVE DA POWAH");
            let res = unsafe { controller.as_ref().controllerStateMachine().inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe { controller.as_ref().controllerStateMachine().respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::ControllerStart => {
            println!("STARTING ENGINGES!");
            let res = unsafe { controller.as_ref().controllerStateMachine().inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe { controller.as_ref().controllerStateMachine().respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::ControllerPause => {
            println!("PAUSING!");
            let res = unsafe { controller.as_ref().controllerStateMachine().inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe { controller.as_ref().controllerStateMachine().respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::PortPowerOn => {
            println!("PORT POWAH!");

            let mut err: Option<Retained<NSError>> = None;

            let res = unsafe { controller.as_ref().getPortStateMachineForCommand_error(NonNull::from(&command), Some(&mut err)) };
            if res.is_none() {
                panic!("Error: {:?}", err);
            }

            let port = res.unwrap();
            let res = unsafe { port.inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
            let res = unsafe { port.respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            unsafe { port.setConnected(true) };
        }
        IOUSBHostCIMessageType::PortStatus => {
            println!("PORT STATUS!");
            let mut err: Option<Retained<NSError>> = None;

            let res = unsafe { controller.as_ref().getPortStateMachineForCommand_error(NonNull::from(&command), Some(&mut err)) };
            if res.is_none() {
                panic!("Error: {:?}", err);
            }

            let port = res.unwrap();
            let res = unsafe { port.inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
            let res = unsafe { port.respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::PortReset => {
            println!("PORT RESET!");
            let mut err: Option<Retained<NSError>> = None;

            let res = unsafe { controller.as_ref().getPortStateMachineForCommand_error(NonNull::from(&command), Some(&mut err)) };
            if res.is_none() {
                panic!("Error: {:?}", err);
            }

            let port = res.unwrap();
            let res = unsafe { port.inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe { port.updateLinkState_speed_inhibitLinkStateChange_error(IOUSBHostCILinkState::U0, IOUSBHostCIDeviceSpeed::Low, false) };
            if res.is_err() {
                panic!("Error: {:?}", err);
            }

            let res = unsafe { port.respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::DeviceCreate => {
            println!("DEVICE CREATE!");
            let mut err: Option<Retained<NSError>> = None;

            let dev = IOUSBHostCIDeviceStateMachine::alloc();
            let res = unsafe { IOUSBHostCIDeviceStateMachine::initWithInterface_command_error(dev, controller.as_ref(), NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", err);
            }
            let dev = res.unwrap();

            let res = unsafe { dev.respondToCommand_status_deviceAddress_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success, 1) }; // TODO: change address
            if res.is_err() {
                panic!("Error: {:?}", err);
            }

            devices.lock().unwrap().push(Device {
                device: dev,
                endpoints: Vec::new(),
            });
        }
        IOUSBHostCIMessageType::EndpointCreate => {
            println!("ENDPOINT CREATE!");

            // TODO: get device address and pick device from it

            let ep = IOUSBHostCIEndpointStateMachine::alloc();
            let res = unsafe { IOUSBHostCIEndpointStateMachine::initWithInterface_command_error(ep, controller.as_ref(), NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
            let ep = res.unwrap();

            let mut dev = devices.lock().unwrap();
            let dev = dev.last_mut().unwrap();

            let res = unsafe { ep.respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) }; // TODO: change address
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            dev.endpoints.push(ep);
        }
        IOUSBHostCIMessageType::EndpointSetNextTransfer => {
            println!("ENDPOINT SET NEXT TRANSFER!");

            let device_adress: u8 = u8::try_from((command.data0 & IOUSBHostCICommandMessageData0DeviceAddress) >> IOUSBHostCICommandMessageData0DeviceAddressPhase).unwrap();
            let endpoint_address: u8 = u8::try_from((command.data0 & IOUSBHostCICommandMessageData0EndpointAddress) >> IOUSBHostCICommandMessageData0EndpointAddressPhase).unwrap();

            let lock = devices.lock().unwrap();
            let dev = lock.iter().find(|dev| {
                let address = unsafe { u8::try_from(dev.device.deviceAddress()).unwrap() };
                address == device_adress
            }).unwrap();

            let ep = dev.endpoints.iter().find(|ep| {
                let address = unsafe { u8::try_from(ep.endpointAddress()).unwrap() };
                address == endpoint_address
            }).unwrap();

            let res = unsafe { ep.inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe { ep.respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) }; // TODO: change address
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        _ => {
            panic!("Unknown message type");
        }
    }
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

fn doorbell_handler(
    controller: NonNull<IOUSBHostControllerInterface>,
    doorbellArray: NonNull<IOUSBHostCIDoorbell>,
    doorbellCount: u32
) {
    println!("Doorbell handler called with doorbell: {:?}", doorbellArray);

    let doorbells = NonNull::slice_from_raw_parts(doorbellArray, doorbellCount as usize);

    for db in unsafe { doorbells.as_ref() } {
        let device_adress: u8 = u8::try_from((db & IOUSBHostCIDoorbellDeviceAddress) >> IOUSBHostCIDoorbellDeviceAddressPhase).unwrap();
        let endpoint_address: u8 = u8::try_from((db & IOUSBHostCIDoorbellEndpointAddress) >> IOUSBHostCIDoorbellEndpointAddressPhase).unwrap();
        let stream_id: u8 = u8::try_from((db & IOUSBHostCIDoorbellStreamID) >> IOUSBHostCIDoorbellStreamIDPhase).unwrap();

        println!("Doorbell device address: {:02x}, endpoint address: {:02x}, stream id: {:02x}", device_adress, endpoint_address, stream_id);

        let lock = devices.lock().unwrap();
        let dev = lock.iter().find(|dev| {
            let address = unsafe { u8::try_from(dev.device.deviceAddress()).unwrap() };
            address == device_adress
        }).unwrap();

        let ep = dev.endpoints.iter().find(|ep| {
            let address = unsafe { u8::try_from(ep.endpointAddress()).unwrap() };
            address == endpoint_address
        }).unwrap();

        let res = unsafe { ep.processDoorbell_error(*db) };
        if res.is_err() {
            panic!("Error: {:?}", res.err().unwrap());
        }

        let msg = unsafe { ep.currentTransferMessage() };
        let msg = unsafe { msg.as_ref() };
        let msg_type = IOUSBHostCIMessageType((msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
        println!("MSG type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

        match msg_type {
            IOUSBHostCIMessageType::SetupTransfer => {
                println!("Setup Transfer");
                
                if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
                    println!("No response needed...");
                    return;
                }

                let requestType: u8 = ((msg.data1 & IOUSBHostCISetupTransferData1bmRequestType) >> IOUSBHostCISetupTransferData1bmRequestTypePhase).try_into().unwrap();
                let request: u8 = ((msg.data1 & IOUSBHostCISetupTransferData1bRequest) >> IOUSBHostCISetupTransferData1bRequestPhase).try_into().unwrap();
                let value: u16 = ((msg.data1 & IOUSBHostCISetupTransferData1wValue) >> IOUSBHostCISetupTransferData1wValuePhase).try_into().unwrap();
                let index: u16 = ((msg.data1 & IOUSBHostCISetupTransferData1wIndex) >> IOUSBHostCISetupTransferData1wIndexPhase).try_into().unwrap();
                let length: u16 = ((msg.data1 & IOUSBHostCISetupTransferData1wLength) >> IOUSBHostCISetupTransferData1wLengthPhase).try_into().unwrap();

                println!("Setup Transfer: requestType: {:02x}, request: {:02x}, value: {:02x}, index: {:02x}, length: {:02x}", requestType, request, value, index, length);

                match requestType {
                    _ if requestType == (UsbDirection::USB_DIR_IN as u8 | UsbType::USB_TYPE_STANDARD as u8 | UsbRecipient::USB_RECIP_DEVICE as u8) => {
                        match request {
                            0x06 => {
                                println!("GET_DESCRIPTOR");
                                println!("Descriptor type: {:02x}", value >> 8);
                                println!("Descriptor index: {:02x}", value & 0x00FF);   
                                println!("Descriptor length: {:02x}", length);  
                                get_descriptor(unsafe { controller.as_ref() }, ep, value);
                            }
                            _ => {
                                println!("Unknown request: {:02x}", request);
                            }
                        }
                    }
                    _ if requestType == (UsbDirection::USB_DIR_OUT as u8 | UsbType::USB_TYPE_STANDARD as u8 | UsbRecipient::USB_RECIP_DEVICE as u8) => {
                        panic!("Request type: USB_DIR_OUT | USB_TYPE_STANDARD | USB_RECIP_DEVICE");
                    }
                    _ => {
                        panic!("Unknown request type");
                    }
                }
            }
            _ => {
                panic!("Unknown message type {:?}", msg_type);
            }
        }
    }
}

fn get_descriptor(
    controller: &IOUSBHostControllerInterface,
    ep: &IOUSBHostCIEndpointStateMachine,
    value: u16,
) {
    let desc_type = (value >> 8) as u8;
    let desc_index = (value & 0x00FF) as u8;

    let scratch: &[u8];

    let mut scratch_page: Vec<u8> = vec![];

    match desc_type {
        0x01 => {
            println!("Device descriptor");
            scratch = unsafe { from_raw_parts(&device_desc as *const _ as *const u8, mem::size_of::<UsbDescDevice>()) };
        }
        0x03 => {
            println!("String descriptor {:02x}", desc_index);
            let string = match desc_index {
                1 => "Carlosless Chedar",
                2 => "USBIP Device",
                3 => "1234567890",
                _ => panic!("Unknown string descriptor index: {}", desc_index),
            };
            let len = string.len() * 2 + 2;
            scratch_page.resize(len, 0);
            scratch_page[0] = string.len() as u8 * 2 + 2;
            scratch_page[1] = UsbDescriptor::USB_DESC_STRING as u8;
            scratch_page[2..].iter_mut().zip(string.as_bytes().iter().flat_map(|&b| [b, 0])).for_each(|(s, b)| *s = b);
            scratch = &scratch_page;
        }
        _ => {
            panic!("Unknown descriptor type: {:02x}", desc_type);
        }
    }

    let res = unsafe { ep.enqueueTransferCompletionForMessage_status_transferLength_error(ep.currentTransferMessage(), IOUSBHostCIMessageStatus::Success, 0) }; // TODO: check if correct length
    if res.is_err() {
        panic!("Error: {:?}", res.err().unwrap());
    }

    if unsafe { ep.endpointState() } != IOUSBHostCIEndpointState::Active {
        panic!("Endpoint is not active");
    }

    let msg = unsafe { ep.currentTransferMessage().as_ref() };
    if (msg.control & IOUSBHostCIMessageControlValid) == 0 {
        panic!("Message is not valid");
    }

    let msg_type = IOUSBHostCIMessageType((msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
    println!("MSG type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

    if msg_type != IOUSBHostCIMessageType::NormalTransfer {
        panic!("Message is not a normal transfer");
    }

    if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
        panic!("Message should need a response");
    }

    let data_length: u32 = ((msg.data0 & IOUSBHostCINormalTransferData0Length) >> IOUSBHostCINormalTransferData0LengthPhase).try_into().unwrap();
    let buffer: &mut [u8] = unsafe { from_raw_parts_mut(((msg.data1 & IOUSBHostCINormalTransferData1Buffer) >> IOUSBHostCINormalTransferData1BufferPhase) as *mut _, data_length as usize) };

    buffer.copy_from_slice(&scratch[..data_length as usize]);

    let res = unsafe { ep.enqueueTransferCompletionForMessage_status_transferLength_error(NonNull::from(msg), IOUSBHostCIMessageStatus::Success, data_length as usize) };
    if res.is_err() {
        panic!("Error: {:?}", res.err().unwrap());
    }

    let msg = unsafe { ep.currentTransferMessage().as_ref() };
    if (msg.control & IOUSBHostCIMessageControlValid) == 0 {
        panic!("Message is not valid");
    }

    let msg_type = IOUSBHostCIMessageType((msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
    println!("MSG type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

    if msg_type != IOUSBHostCIMessageType::StatusTransfer {
        panic!("Message is not a status transfer");
    }

    if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
        panic!("Message should need a response");
    }

    let res = unsafe { ep.enqueueTransferCompletionForMessage_status_transferLength_error(NonNull::from(msg), IOUSBHostCIMessageStatus::Success, 0) };
    if res.is_err() {
        panic!("Error: {:?}", res.err().unwrap());
    }

}

unsafe extern "C-unwind" fn interest_handler(
    refCon: *mut c_void,
    io_service_t: u32,
    messageType: u32,
    messageArgument:*mut c_void
) {
    println!("Interest handler called with refCon: {:?}, io_service_t: {}, messageType: {}, messageArgument: {:?}", refCon, io_service_t, messageType, messageArgument);
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut CAPABILITIES: IOUSBHostCIMessage = IOUSBHostCIMessage {
        control: (IOUSBHostCIMessageType::ControllerCapabilities.0 << IOUSBHostCIMessageControlTypePhase)
            | IOUSBHostCIMessageControlNoResponse
            | IOUSBHostCIMessageControlValid
            | (1 << IOUSBHostCICapabilitiesMessageControlPortCountPhase), // Port count
        data0: (1 << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase)     // 2 seconds
            | (2 << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),          // 4ms
        data1: 0,
    };

    let mut PORT_CAPABILITIES: IOUSBHostCIMessage = IOUSBHostCIMessage {
        control: (IOUSBHostCIMessageType::PortCapabilities.0 << IOUSBHostCIMessageControlTypePhase)
            | IOUSBHostCIMessageControlNoResponse
            | IOUSBHostCIMessageControlValid
            | (1 << IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase)
            | (0 << IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase),        // ACPI TypeA
        data0: ((907 / 8) << IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase),          // Max power for the power (8mA units)
        data1: 0,
    };

    let command_block= RcBlock::new(command_handler);
    let doorbell_block= RcBlock::new(doorbell_handler);

    let capabilities = unsafe {
        let data = NSMutableData::dataWithLength(0).unwrap();
        data.appendBytes_length(NonNull::new_unchecked(&mut CAPABILITIES as *mut _ as *mut c_void), mem::size_of::<IOUSBHostCIMessage>());
        data.appendBytes_length(NonNull::new_unchecked(&mut PORT_CAPABILITIES as *mut _ as *mut c_void), mem::size_of::<IOUSBHostCIMessage>());
        data
    };

    let mut error: Option<Retained<NSError>> = None;

    let interface = unsafe {
        let interface = IOUSBHostControllerInterface::alloc();

        let interface = IOUSBHostControllerInterface::initWithCapabilities_queue_interruptRateHz_error_commandHandler_doorbellHandler_interestHandler(
            interface,
            &capabilities,
            None,
            1000,
            Some(&mut error),
            block2::RcBlock::as_ptr(&command_block),
            block2::RcBlock::as_ptr(&doorbell_block),
            Some(interest_handler)
        );
        interface
    };

    if interface.is_none() {
        unsafe {
            println!("Failed to create IOUSBHostControllerInterface");
            println!("Error: {:?}", error);
            println!("Error: {:?}", error.as_ref().unwrap().localizedFailureReason().unwrap());
            return Err(Error::new(std::io::ErrorKind::Other, "Failed to create IOUSBHostControllerInterface"));
        }
    }

    dispatch_main();

    connect_to_usbip_server("carlossless-chedar:3240").await?;

    Ok(())
}
