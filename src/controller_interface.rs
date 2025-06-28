use std::{borrow::BorrowMut, cell::{RefCell, UnsafeCell}, cmp::min, ffi::CStr, io::Error, mem, os::raw::c_void, ptr::NonNull, rc::Rc, slice::from_raw_parts_mut, sync::Arc};

use block2::RcBlock;
use dispatch2::run_on_main;
use objc2::{rc::Retained, AnyThread};
use objc2_foundation::{NSError, NSMutableData};
use objc2_io_usb_host::{IOUSBHostCICapabilitiesMessageControlPortCountPhase, IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase, IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase, IOUSBHostCICommandMessageData0DeviceAddress, IOUSBHostCICommandMessageData0DeviceAddressPhase, IOUSBHostCICommandMessageData0EndpointAddress, IOUSBHostCICommandMessageData0EndpointAddressPhase, IOUSBHostCIDeviceSpeed, IOUSBHostCIDeviceStateMachine, IOUSBHostCIDoorbell, IOUSBHostCIDoorbellDeviceAddress, IOUSBHostCIDoorbellDeviceAddressPhase, IOUSBHostCIDoorbellEndpointAddress, IOUSBHostCIDoorbellEndpointAddressPhase, IOUSBHostCIDoorbellStreamID, IOUSBHostCIDoorbellStreamIDPhase, IOUSBHostCIEndpointState, IOUSBHostCIEndpointStateMachine,  IOUSBHostCILinkState, IOUSBHostCIMessage, IOUSBHostCIMessageControlNoResponse, IOUSBHostCIMessageControlStatus, IOUSBHostCIMessageControlStatusPhase, IOUSBHostCIMessageControlType, IOUSBHostCIMessageControlTypePhase, IOUSBHostCIMessageControlValid, IOUSBHostCIMessageStatus, IOUSBHostCIMessageType, IOUSBHostCINormalTransferData0Length, IOUSBHostCINormalTransferData0LengthPhase, IOUSBHostCINormalTransferData1Buffer, IOUSBHostCINormalTransferData1BufferPhase, IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase, IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase, IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase, IOUSBHostCISetupTransferData1bRequest, IOUSBHostCISetupTransferData1bRequestPhase, IOUSBHostCISetupTransferData1bmRequestType, IOUSBHostCISetupTransferData1bmRequestTypePhase, IOUSBHostCISetupTransferData1wIndex, IOUSBHostCISetupTransferData1wIndexPhase, IOUSBHostCISetupTransferData1wLength, IOUSBHostCISetupTransferData1wLengthPhase, IOUSBHostCISetupTransferData1wValue, IOUSBHostCISetupTransferData1wValuePhase, IOUSBHostControllerInterface};
use tokio::runtime::Runtime;

use crate::{device::Device, endpoint::Endpoint, usbip::{UrbTransferFlags, UsbIpClient, UsbIpDirection}};

pub struct ForceableSend<T>(pub T);

unsafe impl<T> Send for ForceableSend<T> {}

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

impl ControllerInterface {
    pub fn new(usbip_client: Arc<UnsafeCell<UsbIpClient>>) -> Result<Self, Error> {
        let mut capabilities: IOUSBHostCIMessage = IOUSBHostCIMessage {
            control: (IOUSBHostCIMessageType::ControllerCapabilities.0 << IOUSBHostCIMessageControlTypePhase)
                | IOUSBHostCIMessageControlNoResponse
                | IOUSBHostCIMessageControlValid
                | (1 << IOUSBHostCICapabilitiesMessageControlPortCountPhase),             // Port count
            data0: (1 << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase) // 2 seconds
                | (2 << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),       // 4ms
            data1: 0,
        };

        let mut port_capabilities: IOUSBHostCIMessage = IOUSBHostCIMessage {
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

        let devices: Rc<RefCell<Vec<Device>>> = Rc::new(RefCell::new(Vec::new()));
        let devices_cmd = Rc::clone(&devices);
        let devices_db = Rc::clone(&devices);

        let usbip_client_cmd = Arc::clone(&usbip_client);
        let usbip_client_db = Arc::clone(&usbip_client);

        let rt = Rc::new(RefCell::new(rt));
        let rt_db = Rc::clone(&rt);

        let command_block= RcBlock::new(fnmut_to_fn2(move |a,b| {
            let dev: &mut Vec<Device> = unsafe { &mut *devices_cmd.as_ptr() };
            command_handler(a,b, dev,Arc::clone(&usbip_client_cmd))
        }));
        let doorbell_block= RcBlock::new(fnmut_to_fn3(move |a,b,c| {
            let dev: &mut Vec<Device> = unsafe { &mut *devices_db.as_ptr() };
            doorbell_handler(a, b, c, dev,  Arc::clone(&usbip_client_db), &rt_db.borrow())
        }));

        let capabilities = unsafe {
            let data = NSMutableData::dataWithLength(0).unwrap();
            data.appendBytes_length(NonNull::new_unchecked(&mut capabilities as *mut _ as *mut c_void), mem::size_of::<IOUSBHostCIMessage>());
            data.appendBytes_length(NonNull::new_unchecked(&mut port_capabilities as *mut _ as *mut c_void), mem::size_of::<IOUSBHostCIMessage>());
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
    _client: Arc<UnsafeCell<UsbIpClient>>,
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

            let res = unsafe { port.updateLinkState_speed_inhibitLinkStateChange_error(IOUSBHostCILinkState::U0, IOUSBHostCIDeviceSpeed::Full, false) };
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
            let err: Option<Retained<NSError>> = None;

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

            println!("================== Endpoint address: {:#02x}", unsafe { ep.endpointAddress() });

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

            println!("Device address: {:02x}, endpoint address: {:02x}", device_adress, endpoint_address);

            let dev = devices.iter().find(|dev| {
                let address = u8::try_from(dev.get_device_address()).unwrap();
                address == device_adress
            }).unwrap();

            let ep = dev.get_endpoints().iter().find(|ep| {
                let address = u8::try_from(ep.get_endpoint_address()).unwrap();
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
                let address = u8::try_from(dev.get_device_address()).unwrap();
                address == device_adress
            }).unwrap();

            let ep = dev.get_endpoints().iter().find(|ep| {
                let address = u8::try_from(ep.get_endpoint_address()).unwrap();
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
    _controller: NonNull<IOUSBHostControllerInterface>,
    doorbell_array: NonNull<IOUSBHostCIDoorbell>,
    doorbell_count: u32,
    devices: &mut Vec<Device>,
    client: Arc<UnsafeCell<UsbIpClient>>,
    rt: &Runtime,
) {
    println!("Doorbell handler called with doorbell: {:?}", doorbell_array);

    let doorbells = NonNull::slice_from_raw_parts(doorbell_array, doorbell_count as usize);

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
        let msg_status = IOUSBHostCIMessageStatus((msg.control & IOUSBHostCIMessageControlStatus) >> IOUSBHostCIMessageControlStatusPhase);
        println!("MSG status: {}", unsafe { CStr::from_ptr(msg_status.to_string()).to_str().unwrap() });

        match msg_type {
            IOUSBHostCIMessageType::SetupTransfer => {
                println!("Setup Transfer");
                
                if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
                    println!("No response needed...");
                    return;
                }

                let setup_bytes = msg.data1;
                let request_type: u8 = ((setup_bytes & IOUSBHostCISetupTransferData1bmRequestType) >> IOUSBHostCISetupTransferData1bmRequestTypePhase).try_into().unwrap();
                let request: u8 = ((setup_bytes & IOUSBHostCISetupTransferData1bRequest) >> IOUSBHostCISetupTransferData1bRequestPhase).try_into().unwrap();
                let value: u16 = ((setup_bytes & IOUSBHostCISetupTransferData1wValue) >> IOUSBHostCISetupTransferData1wValuePhase).try_into().unwrap();
                let index: u16 = ((setup_bytes & IOUSBHostCISetupTransferData1wIndex) >> IOUSBHostCISetupTransferData1wIndexPhase).try_into().unwrap();
                let length: u16 = ((setup_bytes & IOUSBHostCISetupTransferData1wLength) >> IOUSBHostCISetupTransferData1wLengthPhase).try_into().unwrap();

                println!("Setup Transfer: requestType: {:02x}, request: {:02x}, value: {:02x}, index: {:02x}, length: {:02x}", request_type, request, value, index, length);


                let mut cl = Arc::clone(&client);

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

                    // let usbip_dir = if (endpoint_address & 0x80) != 0 { UsbIpDirection::UsbDirOut } else { UsbIpDirection::UsbDirIn };
                    // let data_dir = if (request_type & 0x80) != 0 { UsbIpDirection::UsbDirIn } else { UsbIpDirection::UsbDirOut };

                    let dir = if (request_type & 0x80) != 0 { UsbIpDirection::UsbDirIn } else { UsbIpDirection::UsbDirOut };
                    // let dir = if (request_type & 0x80 as u8) != 0 { 1 } else { 0 };
                    let flags = if (request_type & 0x80 as u8) != 0 { UrbTransferFlags::DirIn } else { UrbTransferFlags::DirOut };

                    println!("Length: {}", data_length);
                    let transfer_buffer = if flags == UrbTransferFlags::DirIn { vec![] } else { buffer.to_vec() };

                    let ret = rt.block_on(async {
                        println!("Submitting setup transfer...");
                        unsafe {
                            cl.borrow_mut().get().as_mut().unwrap().cmd_submit(
                                dir as UsbIpDirection,
                                (endpoint_address & 0x0f) as u32,
                                flags.bits(),
                                length as u32,
                                0,
                                0,
                                0,
                                setup_bytes.to_le_bytes(),
                                &transfer_buffer,
                            ).await
                        }
                    }).unwrap();

                    println!("Submit result: {:?}", ret);

                    let real_length = min(ret.buffer.len(), data_length as usize);
                    if (real_length as u32) != data_length {
                        println!("WARN: Data length mismatch: {} != {}", real_length, data_length);
                    }

                    if flags == UrbTransferFlags::DirIn {
                        println!("Copying data to buffer...");
                        buffer[..real_length].copy_from_slice(&ret.buffer[..real_length as usize]);
                    } else {
                    }

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

                let msg = unsafe { ep.currentTransferMessage().as_ref() };
                if (msg.control & IOUSBHostCIMessageControlValid) == 0 {
                    let msg_type = IOUSBHostCIMessageType((msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
                    println!("MSG type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

                    if msg_type != IOUSBHostCIMessageType::Link {
                        panic!("Message is not a link");
                    }

                    if msg.control & IOUSBHostCIMessageControlNoResponse == 0 {
                        panic!("Message should not need a response");
                    }
                }
            }
            IOUSBHostCIMessageType::NormalTransfer => {
                if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
                    panic!("Message should need a response");
                }

                if msg.control & IOUSBHostCIMessageControlValid ==  0 {
                    panic!("Message should be valid");
                }


                let data_length: u32 = ((msg.data0 & IOUSBHostCINormalTransferData0Length) >> IOUSBHostCINormalTransferData0LengthPhase).try_into().unwrap();
                let buffer: &mut [u8] = unsafe { from_raw_parts_mut(((msg.data1 & IOUSBHostCINormalTransferData1Buffer) >> IOUSBHostCINormalTransferData1BufferPhase) as *mut _, data_length as usize) };

                println!("Length: {}", data_length);
                println!("Buffer: {:?}", buffer);

                let dir = if (endpoint_address & 0x00000080) != 0 { UsbIpDirection::UsbDirIn } else { UsbIpDirection::UsbDirOut };

                let ep = &ep.get_state_machine();
                let ep_ptr = ForceableSend(ep.clone());
                let client = ForceableSend(client.clone());

                rt.spawn(async move {
                    let transfer_buffer = if dir == UsbIpDirection::UsbDirIn { vec![] } else { buffer.to_vec() };
                    let mut client = client;
                    let ret = unsafe {
                        client.0.borrow_mut().get().as_mut().unwrap().cmd_submit(
                            dir as UsbIpDirection,
                            (endpoint_address & 0x0f) as u32,
                            (if dir == UsbIpDirection::UsbDirIn { UrbTransferFlags::DirIn } else { UrbTransferFlags::DirOut }).bits(),
                            data_length as u32,
                            0,
                            0,
                            0, 
                            [0, 0, 0, 0, 0, 0, 0, 0],
                            &transfer_buffer,
                        ).await
                    }.unwrap();
                    run_on_main(move |_| {
                        println!("Submit result: {:?}", ret);

                        let real_length = min(ret.buffer.len(), data_length as usize);
                        if (real_length as u32) != data_length {
                            println!("WARN: Data length mismatch: {} != {}", real_length, data_length);
                        }

                        if dir == UsbIpDirection::UsbDirIn {
                            println!("Copying data to buffer...");
                            buffer[..real_length].copy_from_slice(&ret.buffer[..real_length as usize]);
                        } else {
                            println!("Copying data from buffer... NOT IMPLEMENTED");
                        }

                        let ep_ptr = ep_ptr;
                        let ep = ep_ptr.0;

                        let res = unsafe { ep.enqueueTransferCompletionForMessage_status_transferLength_error(NonNull::from(msg), IOUSBHostCIMessageStatus::Success, data_length as usize) };
                        if res.is_err() {
                            panic!("Error: {:?}", res.err().unwrap());
                        }

                        let msg = unsafe { ep.currentTransferMessage().as_ref() };

                        let msg_type = IOUSBHostCIMessageType((msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
                        println!("MSG type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

                        if msg_type != IOUSBHostCIMessageType::Link {
                            panic!("Message is not a link");
                        }

                        if msg.control & IOUSBHostCIMessageControlValid != 0 {
                            // panic!("Message should be valid");
                            println!("Message is valid");
                            return;
                        }

                        println!("======== Needs response? {}", msg.control & IOUSBHostCIMessageControlNoResponse != 0);
                        if msg.control & IOUSBHostCIMessageControlNoResponse == 0 {

                            let res = unsafe { ep.enqueueTransferCompletionForMessage_status_transferLength_error(NonNull::from(msg), IOUSBHostCIMessageStatus::Success, 0) };
                            if res.is_err() {
                                panic!("Error: {:?}", res.err().unwrap());
                            }

                            let msg = unsafe { ep.currentTransferMessage().as_ref() };

                            let msg_type = IOUSBHostCIMessageType((msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
                            println!("MSG type: {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });

                            if msg.control & IOUSBHostCIMessageControlValid != 0 {
                                println!("Message is valid");
                                return;
                            }
                        }
                    });
                });
            }
            _ => {
                panic!("Unknown message type {}", unsafe { CStr::from_ptr(msg_type.to_string()).to_str().unwrap() });
            }
        }
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
