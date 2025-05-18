use std::{cell::RefCell, cmp::min, ffi::CStr, io::Error, mem, os::raw::c_void, ptr::NonNull, rc::Rc, slice::{from_raw_parts, from_raw_parts_mut}};

use block2::RcBlock;
use objc2::{rc::Retained, AnyThread};
use objc2_foundation::{NSError, NSMutableData};
use objc2_io_usb_host::{IOUSBHostCICapabilitiesMessageControlPortCountPhase, IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase, IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase, IOUSBHostCICommandMessageData0DeviceAddress, IOUSBHostCICommandMessageData0DeviceAddressPhase, IOUSBHostCICommandMessageData0EndpointAddress, IOUSBHostCICommandMessageData0EndpointAddressPhase, IOUSBHostCIDeviceSpeed, IOUSBHostCIDeviceStateMachine, IOUSBHostCIDoorbell, IOUSBHostCIDoorbellDeviceAddress, IOUSBHostCIDoorbellDeviceAddressPhase, IOUSBHostCIDoorbellEndpointAddress, IOUSBHostCIDoorbellEndpointAddressPhase, IOUSBHostCIDoorbellStreamID, IOUSBHostCIDoorbellStreamIDPhase, IOUSBHostCIEndpointState, IOUSBHostCIEndpointStateMachine, IOUSBHostCILinkState, IOUSBHostCIMessage, IOUSBHostCIMessageControlNoResponse, IOUSBHostCIMessageControlType, IOUSBHostCIMessageControlTypePhase, IOUSBHostCIMessageControlValid, IOUSBHostCIMessageStatus, IOUSBHostCIMessageType, IOUSBHostCINormalTransferData0Length, IOUSBHostCINormalTransferData0LengthPhase, IOUSBHostCINormalTransferData1Buffer, IOUSBHostCINormalTransferData1BufferPhase, IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase, IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase, IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase, IOUSBHostCISetupTransferData1bRequest, IOUSBHostCISetupTransferData1bRequestPhase, IOUSBHostCISetupTransferData1bmRequestType, IOUSBHostCISetupTransferData1bmRequestTypePhase, IOUSBHostCISetupTransferData1wIndex, IOUSBHostCISetupTransferData1wIndexPhase, IOUSBHostCISetupTransferData1wLength, IOUSBHostCISetupTransferData1wLengthPhase, IOUSBHostCISetupTransferData1wValue, IOUSBHostCISetupTransferData1wValuePhase, IOUSBHostControllerInterface};
use tokio::runtime::Runtime;

use crate::{device::Device, endpoint::Endpoint, usbip::{UsbCommandHeader, UsbCommandSubmit, UsbIpClient}, UsbDesc, UsbDescConfiguration, UsbDescDevice, UsbDescInterface, UsbDescriptor, UsbDirection, UsbIface, UsbRecipient, UsbType};

pub struct ControllerInterface {
    control_interface: Retained<IOUSBHostControllerInterface>,
    devices: Rc<RefCell<Vec<Device>>>,
    rt: Rc<RefCell<Runtime>>,
}

fn fnmut_to_fn2<T,U>(closure: impl FnMut(T, U)) -> impl Fn(T, U) {
    let cell = RefCell::new(closure);

    move |a, b| {
        let mut closure = cell.try_borrow_mut().expect("re-entrant call");
        (closure)(a, b)
    }
}

fn fnmut_to_fn3<T,U,V>(closure: impl FnMut(T, U, V)) -> impl Fn(T, U, V) {
    let cell = RefCell::new(closure);

    move |a, b, c| {
        let mut closure = cell.try_borrow_mut().expect("re-entrant call");
        (closure)(a, b, c)
    }
}

const DEVICE_DESC: UsbDescDevice = UsbDescDevice {
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

const USB_DESC_INTERFACE_MAIN: UsbDescInterface = UsbDescInterface {
    bLength: mem::size_of::<UsbDescInterface>() as u8,
    bDescriptorType: UsbDescriptor::USB_DESC_INTERFACE as u8,
    bInterfaceNumber: 0,
    bAlternateSetting: 0,
    bNumEndpoints: 1,
    bInterfaceClass: UsbIface::USB_IFACE_CLASS_HID as u8,
    bInterfaceSubClass: UsbIface::USB_IFACE_SUBCLASS_HID_BOOT as u8,
    bInterfaceProtocol: 0 as u8, // lack enum
    iInterface: 0,
};

const USB_DESC: UsbDesc = UsbDesc {
    usb_desc: UsbDescConfiguration {
        bLength: mem::size_of::<UsbDescConfiguration>() as u8,
        bDescriptorType: UsbDescriptor::USB_DESC_CONFIGURATION as u8,
        wTotalLength: 0,
        bNumInterfaces: 1,
        bConfigurationValue: 1,
        iConfiguration: 0,
        bmAttributes: 0x80, // Bus powered
        bMaxPower: 50, // 100mA
    },
    usb_desc_interface: USB_DESC_INTERFACE_MAIN,
};

impl ControllerInterface {
    pub fn new(usbip_client: UsbIpClient) -> Result<Self, Error> {
        let mut CAPABILITIES: IOUSBHostCIMessage = IOUSBHostCIMessage {
            control: (IOUSBHostCIMessageType::ControllerCapabilities.0 << IOUSBHostCIMessageControlTypePhase)
                | IOUSBHostCIMessageControlNoResponse
                | IOUSBHostCIMessageControlValid
                | (1 << IOUSBHostCICapabilitiesMessageControlPortCountPhase),             // Port count
            data0: (1 << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase) // 2 seconds
                | (2 << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),       // 4ms
            data1: 0,
        };

        let mut PORT_CAPABILITIES: IOUSBHostCIMessage = IOUSBHostCIMessage {
            control: (IOUSBHostCIMessageType::PortCapabilities.0 << IOUSBHostCIMessageControlTypePhase)
                | IOUSBHostCIMessageControlNoResponse
                | IOUSBHostCIMessageControlValid
                | (1 << IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase)
                | (0 << IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase),   // ACPI TypeA
            data0: ((907 / 8) << IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase), // Max power for the power (8mA units)
            data1: 0,
        };

        let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

        let devices = Rc::new(RefCell::new(Vec::new()));
        let devices_cmd = Rc::clone(&devices);
        let devices_db = Rc::clone(&devices);

        let usbip_client = Rc::new(RefCell::new(usbip_client));
        let usbip_client_cmd = Rc::clone(&usbip_client);
        let usbip_client_db = Rc::clone(&usbip_client);

        let rt = Rc::new(RefCell::new(rt));
        let rt_db = Rc::clone(&rt);

        let command_block= RcBlock::new(fnmut_to_fn2(move |a,b| {
            command_handler(a,b, devices_cmd.borrow_mut().as_mut(), &usbip_client_cmd.borrow())
        }));
        let doorbell_block= RcBlock::new(fnmut_to_fn3(move |a,b,c| {
            doorbell_handler(a, b, c, devices_db.borrow_mut().as_mut(), &mut usbip_client_db.borrow_mut(), &rt_db.borrow())
        }));

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

        Ok(Self {
            control_interface: interface.unwrap(),
            devices: devices,
            rt,
        })
    }
}

fn command_handler(
    controller: NonNull<IOUSBHostControllerInterface>,
    command: IOUSBHostCIMessage,
    devices: &mut Vec<Device>,
    client: &UsbIpClient,
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

            devices.push(Device::new(dev));
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

            let dev = devices.last_mut().unwrap();

            let res = unsafe { ep.respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) }; // TODO: change address
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            dev.add_endpoint(Endpoint::new(ep));
        }
        IOUSBHostCIMessageType::EndpointSetNextTransfer => {
            println!("ENDPOINT SET NEXT TRANSFER!");

            let device_adress: u8 = u8::try_from((command.data0 & IOUSBHostCICommandMessageData0DeviceAddress) >> IOUSBHostCICommandMessageData0DeviceAddressPhase).unwrap();
            let endpoint_address: u8 = u8::try_from((command.data0 & IOUSBHostCICommandMessageData0EndpointAddress) >> IOUSBHostCICommandMessageData0EndpointAddressPhase).unwrap();

            let dev = devices.iter().find(|dev| {
                let address = unsafe { u8::try_from(dev.get_device_address()).unwrap() };
                address == device_adress
            }).unwrap();

            let ep = dev.get_endpoints().iter().find(|ep| {
                let address = unsafe { u8::try_from(ep.get_endpoint_address()).unwrap() };
                address == endpoint_address
            }).unwrap();

            let res = unsafe { ep.get_state_machine().inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe { ep.get_state_machine().respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) }; // TODO: change address
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::EndpointPause => {
            println!("ENDPOINT PAUSE!");

            let device_adress: u8 = u8::try_from((command.data0 & IOUSBHostCICommandMessageData0DeviceAddress) >> IOUSBHostCICommandMessageData0DeviceAddressPhase).unwrap();
            let endpoint_address: u8 = u8::try_from((command.data0 & IOUSBHostCICommandMessageData0EndpointAddress) >> IOUSBHostCICommandMessageData0EndpointAddressPhase).unwrap();

            let dev = devices.iter().find(|dev| {
                let address = unsafe { u8::try_from(dev.get_device_address()).unwrap() };
                address == device_adress
            }).unwrap();

            let ep = dev.get_endpoints().iter().find(|ep| {
                let address = unsafe { u8::try_from(ep.get_endpoint_address()).unwrap() };
                address == endpoint_address
            }).unwrap();

            let res = unsafe { ep.get_state_machine().inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe { ep.get_state_machine().respondToCommand_status_error(NonNull::from(&command), IOUSBHostCIMessageStatus::Success) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        _ => {
            panic!("Unknown message type");
        }
    }
}

fn doorbell_handler(
    controller: NonNull<IOUSBHostControllerInterface>,
    doorbellArray: NonNull<IOUSBHostCIDoorbell>,
    doorbellCount: u32,
    devices: &mut Vec<Device>,
    client: &mut UsbIpClient,
    rt: &Runtime,
) {
    println!("Doorbell handler called with doorbell: {:?}", doorbellArray);

    let doorbells = NonNull::slice_from_raw_parts(doorbellArray, doorbellCount as usize);

    for db in unsafe { doorbells.as_ref() } {
        let device_adress: u8 = u8::try_from((db & IOUSBHostCIDoorbellDeviceAddress) >> IOUSBHostCIDoorbellDeviceAddressPhase).unwrap();
        let endpoint_address: u8 = u8::try_from((db & IOUSBHostCIDoorbellEndpointAddress) >> IOUSBHostCIDoorbellEndpointAddressPhase).unwrap();
        let stream_id: u8 = u8::try_from((db & IOUSBHostCIDoorbellStreamID) >> IOUSBHostCIDoorbellStreamIDPhase).unwrap();

        println!("Doorbell device address: {:02x}, endpoint address: {:02x}, stream id: {:02x}", device_adress, endpoint_address, stream_id);

        let dev = devices.iter().find(|dev| {
            let address = u8::try_from(dev.get_device_address()).unwrap();
            address == device_adress
        }).unwrap();

        let ep = dev.get_endpoints().iter().find(|ep| {
            let address = u8::try_from(ep.get_endpoint_address()).unwrap();
            address == endpoint_address
        }).unwrap();

        let res = unsafe { ep.get_state_machine().processDoorbell_error(*db) };
        if res.is_err() {
            panic!("Error: {:?}", res.err().unwrap());
        }

        let msg = unsafe { ep.get_state_machine().currentTransferMessage() };
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

                let mut ret = rt.block_on(client.cmd_submit(UsbCommandSubmit::new(
                    UsbCommandHeader::new(
                        1,
                        0,
                        0,
                        1,
                        endpoint_address as u32,
                    ),
                    0,
                    length as u32,
                    0,
                    0,
                    0,
                    msg.data1.to_le_bytes(),
                ))).unwrap();

                println!("Submit result: {:?}", ret);

                let ep = &ep.get_state_machine();

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

                if msg_type == IOUSBHostCIMessageType::NormalTransfer {
                    if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
                        panic!("Message should need a response");
                    }

                    let data_length: u32 = ((msg.data0 & IOUSBHostCINormalTransferData0Length) >> IOUSBHostCINormalTransferData0LengthPhase).try_into().unwrap();
                    let buffer: &mut [u8] = unsafe { from_raw_parts_mut(((msg.data1 & IOUSBHostCINormalTransferData1Buffer) >> IOUSBHostCINormalTransferData1BufferPhase) as *mut _, data_length as usize) };

                    println!("Length: {}", data_length);

                    let real_length = min(ret.buffer.len(), data_length as usize);
                    if (real_length as u32) != data_length {
                        println!("WARN: Data length mismatch: {} != {}", real_length, data_length);
                    }

                    buffer[..real_length].copy_from_slice(&ret.buffer[..real_length as usize]);

                    let res = unsafe { ep.enqueueTransferCompletionForMessage_status_transferLength_error(NonNull::from(msg), IOUSBHostCIMessageStatus::Success, data_length as usize) };
                    if res.is_err() {
                        panic!("Error: {:?}", res.err().unwrap());
                    }
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
            _ => {
                panic!("Unknown message type {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });
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
            scratch = unsafe { from_raw_parts(&DEVICE_DESC as *const _ as *const u8, mem::size_of::<UsbDescDevice>()) };
        }
        0x02 => {
            println!("Configuration descriptor");
            scratch = unsafe { from_raw_parts(&USB_DESC as *const _ as *const u8, mem::size_of::<UsbDescConfiguration>()) };
        }
        0x03 => {
            println!("String descriptor {:02x}", desc_index);

            if desc_index == 0 {
                scratch_page.resize(6, 0);
                scratch_page[0] = 6;
                scratch_page[1] = UsbDescriptor::USB_DESC_STRING as u8;
                scratch_page[2] = 0x09; // Unicode
                scratch_page[3] = 0x04; // English
                scratch_page[4] = 0x00; // Language ID
                scratch_page[5] = 0x00; // Language ID
                scratch = &scratch_page;
            } else {
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

    let real_length = min(scratch.len(), data_length as usize);
    if (real_length as u32) != data_length {
        println!("WARN: Data length mismatch: {} != {}", real_length, data_length);
    }

    buffer[..real_length].copy_from_slice(&scratch[..real_length as usize]);

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

fn set_configuration(
    controller: &IOUSBHostControllerInterface,
    ep: &IOUSBHostCIEndpointStateMachine,
    value: u16,
) {
    println!("Set configuration: {:02x}", value);

    let msg = unsafe { ep.currentTransferMessage().as_ref() };
    if (msg.control & IOUSBHostCIMessageControlValid) == 0 {
        panic!("Message is not valid");
    }

    let msg_type = IOUSBHostCIMessageType((msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
    println!("MSG type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

    if msg_type != IOUSBHostCIMessageType::SetupTransfer {
        panic!("Message is not a setup transfer");
    }

    if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
        panic!("Message should need a response");
    }

    let res = unsafe { ep.enqueueTransferCompletionForMessage_status_transferLength_error(NonNull::from(msg), IOUSBHostCIMessageStatus::Success, 0) };
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
    ref_con: *mut c_void,
    io_service_t: u32,
    message_type: u32,
    message_argument: *mut c_void
) {
    println!("Interest handler called with ref_con: {:?}, io_service_t: {}, message_type: {}, message_argument: {:?}", ref_con, io_service_t, message_type, message_argument);
}
