use std::{
    borrow::BorrowMut,
    cell::{RefCell, UnsafeCell},
    cmp::min,
    ffi::{c_ulong, CStr},
    io::Error,
    mem,
    os::raw::c_void,
    ptr::NonNull,
    rc::Rc,
    slice::from_raw_parts_mut,
    sync::Arc,
};

use block2::RcBlock;
use dispatch2::run_on_main;
use log::{debug, error, warn};
use objc2::{rc::Retained, AnyThread};
use objc2_foundation::{NSError, NSMutableData};
use objc2_io_usb_host::{
    IOUSBHostCICapabilitiesMessageControlPortCountPhase,
    IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase,
    IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase,
    IOUSBHostCICommandMessageData0DeviceAddress, IOUSBHostCICommandMessageData0DeviceAddressPhase,
    IOUSBHostCICommandMessageData0EndpointAddress,
    IOUSBHostCICommandMessageData0EndpointAddressPhase, IOUSBHostCIDeviceSpeed,
    IOUSBHostCIDeviceStateMachine, IOUSBHostCIDoorbell, IOUSBHostCIDoorbellDeviceAddress,
    IOUSBHostCIDoorbellDeviceAddressPhase, IOUSBHostCIDoorbellEndpointAddress,
    IOUSBHostCIDoorbellEndpointAddressPhase, IOUSBHostCIDoorbellStreamID,
    IOUSBHostCIDoorbellStreamIDPhase, IOUSBHostCIEndpointState, IOUSBHostCIEndpointStateMachine,
    IOUSBHostCILinkState, IOUSBHostCIMessage, IOUSBHostCIMessageControlNoResponse,
    IOUSBHostCIMessageControlStatus, IOUSBHostCIMessageControlStatusPhase,
    IOUSBHostCIMessageControlType, IOUSBHostCIMessageControlTypePhase,
    IOUSBHostCIMessageControlValid, IOUSBHostCIMessageStatus, IOUSBHostCIMessageType,
    IOUSBHostCINormalTransferData0Length, IOUSBHostCINormalTransferData0LengthPhase,
    IOUSBHostCINormalTransferData1Buffer, IOUSBHostCINormalTransferData1BufferPhase,
    IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase,
    IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase,
    IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase, IOUSBHostCISetupTransferData1bRequest,
    IOUSBHostCISetupTransferData1bRequestPhase, IOUSBHostCISetupTransferData1bmRequestType,
    IOUSBHostCISetupTransferData1bmRequestTypePhase, IOUSBHostCISetupTransferData1wIndexPhase,
    IOUSBHostCISetupTransferData1wLengthPhase, IOUSBHostCISetupTransferData1wValue,
    IOUSBHostCISetupTransferData1wValuePhase, IOUSBHostControllerInterface,
};
use tokio::runtime::Runtime;

use crate::{
    device::Device,
    endpoint::Endpoint,
    usbip::{UsbIpClient, UsbIpDirection},
};

// These constants are not present in objc2-io-usb-host, so we define them here
// https://github.com/madsmtm/objc2/issues/753
macro_rules! IOUSBBitRange {
    ($start:expr, $end:expr) => {
        !((1 << $start) - 1) & ((1 << $end) | ((1 << $end) - 1))
    };
}

#[allow(non_upper_case_globals)]
pub const IOUSBHostCISetupTransferData1wIndex: c_ulong = IOUSBBitRange!(32, 47);
#[allow(non_upper_case_globals)]
pub const IOUSBHostCISetupTransferData1wLength: c_ulong = IOUSBBitRange!(48, 63);

const CONTROLLER_TIMEOUT_THRESHOLD: u32 = 1; // 2 seconds
const CONTROLLER_CONNECTION_LATENCY: u32 = 2; // 4ms
const PORT_COUNT: u32 = 1;
const MAX_POWER_MA: u32 = 907; // mA
const POWER_UNIT_SIZE: u32 = 8; // 8mA units

/// A wrapper to force Send trait on types that may not be Send
/// This is used for cross-thread communication with Objective-C objects
pub struct ForceableSend<T>(pub T);

unsafe impl<T> Send for ForceableSend<T> {}
unsafe impl<T> Sync for ForceableSend<T> {}

type CommandHandlerArgs =
    dyn Fn(NonNull<IOUSBHostControllerInterface>, NonNull<IOUSBHostCIDoorbell>, u32);

#[derive(Debug)]
pub struct ControllerInterface {
    _control_interface: Retained<IOUSBHostControllerInterface>,
    _devices: Rc<RefCell<Vec<Device>>>,
    _rt: Rc<RefCell<Runtime>>,
}

/// Convert FnMut to Fn for use with Objective-C blocks
fn fnmut_to_fn2<T, U>(closure: impl FnMut(T, U)) -> impl Fn(T, U) {
    let cell = RefCell::new(closure);
    move |a, b| {
        let mut closure = cell.try_borrow_mut().expect("re-entrant call");
        (closure)(a, b)
    }
}

/// Convert FnMut to Fn for use with Objective-C blocks
fn fnmut_to_fn3<T, U, V>(closure: impl FnMut(T, U, V)) -> impl Fn(T, U, V) {
    let cell = RefCell::new(closure);
    move |a, b, c| {
        let mut closure = cell.try_borrow_mut().expect("re-entrant call");
        (closure)(a, b, c)
    }
}

impl ControllerInterface {
    pub fn new(usbip_client: Arc<UnsafeCell<UsbIpClient>>) -> Result<Self, Error> {
        let mut capabilities = Self::create_controller_capabilities();
        let mut port_capabilities = Self::create_port_capabilities();

        let rt = Self::create_runtime()?;
        let devices = Rc::new(RefCell::new(Vec::new()));

        let (devices_cmd, devices_db) = (Rc::clone(&devices), Rc::clone(&devices));
        let (usbip_client_cmd, usbip_client_db) =
            (Arc::clone(&usbip_client), Arc::clone(&usbip_client));
        let rt = Rc::new(RefCell::new(rt));
        let rt_db = Rc::clone(&rt);

        let command_block = Self::create_command_block(devices_cmd, usbip_client_cmd);
        let doorbell_block = Self::create_doorbell_block(devices_db, usbip_client_db, rt_db);

        let capabilities_data =
            Self::create_capabilities_data(&mut capabilities, &mut port_capabilities)?;

        let interface =
            Self::create_controller_interface(&capabilities_data, &command_block, &doorbell_block)?;

        Ok(Self {
            _control_interface: interface,
            _devices: devices,
            _rt: rt,
        })
    }

    fn create_controller_capabilities() -> IOUSBHostCIMessage {
        IOUSBHostCIMessage {
            control: (IOUSBHostCIMessageType::ControllerCapabilities.0
                << IOUSBHostCIMessageControlTypePhase)
                | IOUSBHostCIMessageControlNoResponse
                | IOUSBHostCIMessageControlValid
                | (PORT_COUNT << IOUSBHostCICapabilitiesMessageControlPortCountPhase),
            data0: (CONTROLLER_TIMEOUT_THRESHOLD
                << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase)
                | (CONTROLLER_CONNECTION_LATENCY
                    << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),
            data1: 0,
        }
    }

    fn create_port_capabilities() -> IOUSBHostCIMessage {
        IOUSBHostCIMessage {
            control: (IOUSBHostCIMessageType::PortCapabilities.0
                << IOUSBHostCIMessageControlTypePhase)
                | IOUSBHostCIMessageControlNoResponse
                | IOUSBHostCIMessageControlValid
                | (1 << IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase)
                | (0 << IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase), // ACPI TypeA
            data0: ((MAX_POWER_MA / POWER_UNIT_SIZE)
                << IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase),
            data1: 0,
        }
    }

    fn create_runtime() -> Result<tokio::runtime::Runtime, Error> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::other(format!("Failed to create runtime: {}", e)))
    }

    fn create_command_block(
        devices: Rc<RefCell<Vec<Device>>>,
        usbip_client: Arc<UnsafeCell<UsbIpClient>>,
    ) -> RcBlock<dyn Fn(NonNull<IOUSBHostControllerInterface>, IOUSBHostCIMessage)> {
        RcBlock::new(fnmut_to_fn2(move |a, b| {
            let dev: &mut Vec<Device> = unsafe { &mut *devices.as_ptr() };
            command_handler(a, b, dev, Arc::clone(&usbip_client))
        }))
    }

    fn create_doorbell_block(
        devices: Rc<RefCell<Vec<Device>>>,
        usbip_client: Arc<UnsafeCell<UsbIpClient>>,
        rt: Rc<RefCell<Runtime>>,
    ) -> RcBlock<CommandHandlerArgs> {
        RcBlock::new(fnmut_to_fn3(move |a, b, c| {
            let dev: &mut Vec<Device> = unsafe { &mut *devices.as_ptr() };
            doorbell_handler(a, b, c, dev, Arc::clone(&usbip_client), &rt.borrow())
        }))
    }

    fn create_capabilities_data(
        capabilities: &mut IOUSBHostCIMessage,
        port_capabilities: &mut IOUSBHostCIMessage,
    ) -> Result<Retained<NSMutableData>, Error> {
        unsafe {
            let data = NSMutableData::dataWithLength(0)
                .ok_or_else(|| Error::other("Failed to create NSMutableData"))?;

            data.appendBytes_length(
                NonNull::new_unchecked(capabilities as *mut _ as *mut c_void),
                mem::size_of::<IOUSBHostCIMessage>(),
            );
            data.appendBytes_length(
                NonNull::new_unchecked(port_capabilities as *mut _ as *mut c_void),
                mem::size_of::<IOUSBHostCIMessage>(),
            );
            Ok(data)
        }
    }

    fn create_controller_interface(
        capabilities: &Retained<NSMutableData>,
        command_block: &RcBlock<dyn Fn(NonNull<IOUSBHostControllerInterface>, IOUSBHostCIMessage)>,
        doorbell_block: &RcBlock<CommandHandlerArgs>,
    ) -> Result<Retained<IOUSBHostControllerInterface>, Error> {
        let mut error: Option<Retained<NSError>> = None;

        let interface = unsafe {
            IOUSBHostControllerInterface::initWithCapabilities_queue_interruptRateHz_error_commandHandler_doorbellHandler_interestHandler(
                IOUSBHostControllerInterface::alloc(),
                capabilities,
                None,
                1000,
                Some(&mut error),
                block2::RcBlock::as_ptr(command_block),
                block2::RcBlock::as_ptr(doorbell_block),
                Some(interest_handler)
            )
        };

        interface.ok_or_else(|| {
            if let Some(err) = error {
                let reason = unsafe { err.localizedFailureReason() }
                    .map(|r| format!(": {}", r))
                    .unwrap_or_default();
                error!("Failed to create IOUSBHostControllerInterface{}", reason);
            }
            Error::other("Failed to create IOUSBHostControllerInterface")
        })
    }
}

fn command_handler(
    controller: NonNull<IOUSBHostControllerInterface>,
    command: IOUSBHostCIMessage,
    devices: &mut Vec<Device>,
    _client: Arc<UnsafeCell<UsbIpClient>>,
) {
    debug!("Command handler called with command: {:?}", command);
    let msg_type = IOUSBHostCIMessageType(
        (command.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase,
    );
    debug!("Command type: {}", unsafe {
        CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
    });

    match msg_type {
        IOUSBHostCIMessageType::ControllerPowerOn => {
            debug!("Powering on controller...");
            let res = unsafe {
                controller
                    .as_ref()
                    .controllerStateMachine()
                    .inspectCommand_error(NonNull::from(&command))
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe {
                controller
                    .as_ref()
                    .controllerStateMachine()
                    .respondToCommand_status_error(
                        NonNull::from(&command),
                        IOUSBHostCIMessageStatus::Success,
                    )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::ControllerStart => {
            let res = unsafe {
                controller
                    .as_ref()
                    .controllerStateMachine()
                    .inspectCommand_error(NonNull::from(&command))
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe {
                controller
                    .as_ref()
                    .controllerStateMachine()
                    .respondToCommand_status_error(
                        NonNull::from(&command),
                        IOUSBHostCIMessageStatus::Success,
                    )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::ControllerPause => {
            let res = unsafe {
                controller
                    .as_ref()
                    .controllerStateMachine()
                    .inspectCommand_error(NonNull::from(&command))
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe {
                controller
                    .as_ref()
                    .controllerStateMachine()
                    .respondToCommand_status_error(
                        NonNull::from(&command),
                        IOUSBHostCIMessageStatus::Success,
                    )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::PortPowerOn => {
            let res = unsafe {
                controller
                    .as_ref()
                    .getPortStateMachineForCommand_error(NonNull::from(&command))
            };
            if let Err(err) = res {
                panic!("Error: {:?}", err);
            }

            let port = res.unwrap();
            let res = unsafe { port.inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
            let res = unsafe {
                port.respondToCommand_status_error(
                    NonNull::from(&command),
                    IOUSBHostCIMessageStatus::Success,
                )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            unsafe { port.setConnected(true) };
        }
        IOUSBHostCIMessageType::PortStatus => {
            let res = unsafe {
                controller
                    .as_ref()
                    .getPortStateMachineForCommand_error(NonNull::from(&command))
            };
            if let Err(err) = res {
                panic!("Error: {:?}", err);
            }

            let port = res.unwrap();
            let res = unsafe { port.inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
            let res = unsafe {
                port.respondToCommand_status_error(
                    NonNull::from(&command),
                    IOUSBHostCIMessageStatus::Success,
                )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::PortReset => {
            let res = unsafe {
                controller
                    .as_ref()
                    .getPortStateMachineForCommand_error(NonNull::from(&command))
            };
            if let Err(err) = res {
                panic!("Error: {:?}", err);
            }

            let port = res.unwrap();
            let res = unsafe { port.inspectCommand_error(NonNull::from(&command)) };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe {
                port.updateLinkState_speed_inhibitLinkStateChange_error(
                    IOUSBHostCILinkState::U0,
                    IOUSBHostCIDeviceSpeed::Full,
                    false,
                )
            };
            if let Err(err) = res {
                panic!("Error: {:?}", err);
            }

            let res = unsafe {
                port.respondToCommand_status_error(
                    NonNull::from(&command),
                    IOUSBHostCIMessageStatus::Success,
                )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::DeviceCreate => {
            let res = unsafe {
                IOUSBHostCIDeviceStateMachine::initWithInterface_command_error(
                    IOUSBHostCIDeviceStateMachine::alloc(),
                    controller.as_ref(),
                    NonNull::from(&command),
                )
            };
            if let Err(err) = res {
                panic!("Error: {:?}", err);
            }
            let dev = res.unwrap();

            let res = unsafe {
                dev.respondToCommand_status_deviceAddress_error(
                    NonNull::from(&command),
                    IOUSBHostCIMessageStatus::Success,
                    devices.len() + 1,
                )
            };
            if let Err(err) = res {
                panic!("Error: {:?}", err);
            }

            debug!("Device created with address: {:02x}", unsafe {
                dev.deviceAddress()
            });

            devices.push(Device::new(dev));
        }
        IOUSBHostCIMessageType::EndpointCreate => {
            debug!("ENDPOINT CREATE!");

            let res = unsafe {
                IOUSBHostCIEndpointStateMachine::initWithInterface_command_error(
                    IOUSBHostCIEndpointStateMachine::alloc(),
                    controller.as_ref(),
                    NonNull::from(&command),
                )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
            let ep = res.unwrap();

            let res = unsafe {
                ep.respondToCommand_status_error(
                    NonNull::from(&command),
                    IOUSBHostCIMessageStatus::Success,
                )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let device_address = unsafe { ep.deviceAddress() };

            let dev = devices
                .iter_mut()
                .find(|dev| {
                    let address = dev.device_address();
                    address == device_address
                })
                .unwrap();

            debug!(
                "Endpoint created for device with address: {:02x}",
                device_address
            );

            dev.add_endpoint(Endpoint::new(ep));
        }
        IOUSBHostCIMessageType::EndpointSetNextTransfer => {
            debug!("ENDPOINT SET NEXT TRANSFER!");

            let device_adress: u8 = u8::try_from(
                (command.data0 & IOUSBHostCICommandMessageData0DeviceAddress)
                    >> IOUSBHostCICommandMessageData0DeviceAddressPhase,
            )
            .unwrap();
            let endpoint_address: u8 = u8::try_from(
                (command.data0 & IOUSBHostCICommandMessageData0EndpointAddress)
                    >> IOUSBHostCICommandMessageData0EndpointAddressPhase,
            )
            .unwrap();

            debug!(
                "Device address: {:02x}, endpoint address: {:02x}",
                device_adress, endpoint_address
            );

            let dev = devices
                .iter()
                .find(|dev| {
                    let address = u8::try_from(dev.device_address()).unwrap();
                    address == device_adress
                })
                .unwrap();

            let ep = dev
                .endpoints()
                .iter()
                .find(|ep| {
                    let address = u8::try_from(ep.endpoint_address()).unwrap();
                    address == endpoint_address
                })
                .unwrap();

            let res = unsafe {
                ep.clone_state_machine()
                    .inspectCommand_error(NonNull::from(&command))
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe {
                ep.clone_state_machine().respondToCommand_status_error(
                    NonNull::from(&command),
                    IOUSBHostCIMessageStatus::Success,
                )
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }
        }
        IOUSBHostCIMessageType::EndpointPause => {
            debug!("ENDPOINT PAUSE!");

            let device_address: u8 = u8::try_from(
                (command.data0 & IOUSBHostCICommandMessageData0DeviceAddress)
                    >> IOUSBHostCICommandMessageData0DeviceAddressPhase,
            )
            .unwrap();
            let endpoint_address: u8 = u8::try_from(
                (command.data0 & IOUSBHostCICommandMessageData0EndpointAddress)
                    >> IOUSBHostCICommandMessageData0EndpointAddressPhase,
            )
            .unwrap();

            // FIXME: select devices by address as index
            let dev = devices
                .iter()
                .find(|dev| {
                    let address = u8::try_from(dev.device_address()).unwrap();
                    address == device_address
                })
                .unwrap();

            // FIXME: select devices by address as index
            let ep = dev
                .endpoints()
                .iter()
                .find(|ep| {
                    let address = u8::try_from(ep.endpoint_address()).unwrap();
                    address == endpoint_address
                })
                .unwrap();

            let res = unsafe {
                ep.clone_state_machine()
                    .inspectCommand_error(NonNull::from(&command))
            };
            if res.is_err() {
                panic!("Error: {:?}", res.err().unwrap());
            }

            let res = unsafe {
                ep.clone_state_machine().respondToCommand_status_error(
                    NonNull::from(&command),
                    IOUSBHostCIMessageStatus::Success,
                )
            };
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
    devices: &mut [Device],
    client: Arc<UnsafeCell<UsbIpClient>>,
    rt: &Runtime,
) {
    debug!(
        "Doorbell handler called with doorbell: {:?}",
        doorbell_array
    );

    let doorbells = NonNull::slice_from_raw_parts(doorbell_array, doorbell_count as usize);

    for db in unsafe { doorbells.as_ref() } {
        let device_adress: u8 = u8::try_from(
            (db & IOUSBHostCIDoorbellDeviceAddress) >> IOUSBHostCIDoorbellDeviceAddressPhase,
        )
        .unwrap();
        let endpoint_address: u8 = u8::try_from(
            (db & IOUSBHostCIDoorbellEndpointAddress) >> IOUSBHostCIDoorbellEndpointAddressPhase,
        )
        .unwrap();
        let stream_id: u8 =
            u8::try_from((db & IOUSBHostCIDoorbellStreamID) >> IOUSBHostCIDoorbellStreamIDPhase)
                .unwrap();

        debug!(
            "Doorbell device address: {:02x}, endpoint address: {:02x}, stream id: {:02x}",
            device_adress, endpoint_address, stream_id
        );

        let dev = devices
            .iter()
            .find(|dev| {
                let address = u8::try_from(dev.device_address()).unwrap();
                address == device_adress
            })
            .unwrap();

        let ep = dev
            .endpoints()
            .iter()
            .find(|ep| {
                let address = u8::try_from(ep.endpoint_address()).unwrap();
                address == endpoint_address
            })
            .unwrap();

        let res = unsafe { ep.clone_state_machine().processDoorbell_error(*db) };
        if res.is_err() {
            panic!("Error: {:?}", res.err().unwrap());
        }

        let msg = unsafe { ep.clone_state_machine().currentTransferMessage() };
        let msg = unsafe { msg.as_ref() };
        let msg_type = IOUSBHostCIMessageType(
            (msg.control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase,
        );
        debug!("MSG type: {}", unsafe {
            CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
        });
        let msg_status = IOUSBHostCIMessageStatus(
            (msg.control & IOUSBHostCIMessageControlStatus) >> IOUSBHostCIMessageControlStatusPhase,
        );
        debug!("MSG status: {}", unsafe {
            CStr::from_ptr(msg_status.to_string()).to_str().unwrap()
        });

        match msg_type {
            IOUSBHostCIMessageType::SetupTransfer => {
                if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
                    debug!("No response needed...");
                    return;
                }

                let setup_bytes = msg.data1;
                let request_type: u8 = ((setup_bytes & IOUSBHostCISetupTransferData1bmRequestType)
                    >> IOUSBHostCISetupTransferData1bmRequestTypePhase)
                    .try_into()
                    .unwrap();
                let request: u8 = ((setup_bytes & IOUSBHostCISetupTransferData1bRequest)
                    >> IOUSBHostCISetupTransferData1bRequestPhase)
                    .try_into()
                    .unwrap();
                let value: u16 = ((setup_bytes & IOUSBHostCISetupTransferData1wValue)
                    >> IOUSBHostCISetupTransferData1wValuePhase)
                    .try_into()
                    .unwrap();
                let index: u16 = ((setup_bytes & IOUSBHostCISetupTransferData1wIndex)
                    >> IOUSBHostCISetupTransferData1wIndexPhase)
                    .try_into()
                    .unwrap();
                let length: u16 = ((setup_bytes & IOUSBHostCISetupTransferData1wLength)
                    >> IOUSBHostCISetupTransferData1wLengthPhase)
                    .try_into()
                    .unwrap();

                debug!("Setup Transfer: requestType: {:02x}, request: {:02x}, value: {:02x}, index: {:02x}, length: {:02x}", request_type, request, value, index, length);

                let mut cl = Arc::clone(&client);

                let ep = &ep.clone_state_machine();

                let res = unsafe {
                    ep.enqueueTransferCompletionForMessage_status_transferLength_error(
                        ep.currentTransferMessage(),
                        IOUSBHostCIMessageStatus::Success,
                        0,
                    )
                }; // TODO: check if correct length
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

                let msg_type = IOUSBHostCIMessageType(
                    (msg.control & IOUSBHostCIMessageControlType)
                        >> IOUSBHostCIMessageControlTypePhase,
                );
                debug!("MSG type: {}", unsafe {
                    CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
                });

                if msg_type == IOUSBHostCIMessageType::NormalTransfer {
                    if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
                        panic!("Message should need a response");
                    }

                    let data_length: u32 = (msg.data0 & IOUSBHostCINormalTransferData0Length)
                        >> IOUSBHostCINormalTransferData0LengthPhase;
                    let buffer: &mut [u8] = unsafe {
                        from_raw_parts_mut(
                            ((msg.data1 & IOUSBHostCINormalTransferData1Buffer)
                                >> IOUSBHostCINormalTransferData1BufferPhase)
                                as *mut _,
                            data_length as usize,
                        )
                    };

                    let dir = if (request_type & 0x80) != 0 {
                        UsbIpDirection::UsbDirIn
                    } else {
                        UsbIpDirection::UsbDirOut
                    };

                    debug!("Length: {}", data_length);
                    let transfer_buffer = match dir {
                        UsbIpDirection::UsbDirIn => vec![],
                        UsbIpDirection::UsbDirOut => buffer.to_vec(),
                    };

                    let ret = rt
                        .block_on(async {
                            debug!("Submitting setup transfer...");
                            unsafe {
                                cl.borrow_mut()
                                    .get()
                                    .as_mut()
                                    .unwrap()
                                    .cmd_submit(
                                        dir,
                                        (endpoint_address & 0x0f) as u32,
                                        0,
                                        length as u32,
                                        0,
                                        0,
                                        0,
                                        setup_bytes.to_le_bytes(),
                                        &transfer_buffer,
                                    )
                                    .await
                            }
                        })
                        .unwrap();

                    debug!("Submit result: {:?}", ret);

                    let real_length = min(ret.buffer.len(), data_length as usize);
                    if (real_length as u32) != data_length {
                        debug!(
                            "WARN: Data length mismatch: {} != {}",
                            real_length, data_length
                        );
                    }

                    if dir == UsbIpDirection::UsbDirIn {
                        debug!("Copying data to buffer...");
                        buffer[..real_length].copy_from_slice(&ret.buffer[..real_length as usize]);
                    }

                    let res = unsafe {
                        ep.enqueueTransferCompletionForMessage_status_transferLength_error(
                            NonNull::from(msg),
                            IOUSBHostCIMessageStatus::Success,
                            data_length as usize,
                        )
                    };
                    if res.is_err() {
                        panic!("Error: {:?}", res.err().unwrap());
                    }
                }

                let msg = unsafe { ep.currentTransferMessage().as_ref() };
                if (msg.control & IOUSBHostCIMessageControlValid) == 0 {
                    panic!("Message is not valid");
                }

                let msg_type = IOUSBHostCIMessageType(
                    (msg.control & IOUSBHostCIMessageControlType)
                        >> IOUSBHostCIMessageControlTypePhase,
                );
                debug!("MSG type: {}", unsafe {
                    CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
                });

                if msg_type != IOUSBHostCIMessageType::StatusTransfer {
                    panic!("Message is not a status transfer");
                }

                if msg.control & IOUSBHostCIMessageControlNoResponse != 0 {
                    panic!("Message should need a response");
                }

                let res = unsafe {
                    ep.enqueueTransferCompletionForMessage_status_transferLength_error(
                        NonNull::from(msg),
                        IOUSBHostCIMessageStatus::Success,
                        0,
                    )
                };
                if res.is_err() {
                    panic!("Error: {:?}", res.err().unwrap());
                }

                let msg = unsafe { ep.currentTransferMessage().as_ref() };
                if (msg.control & IOUSBHostCIMessageControlValid) == 0 {
                    let msg_type = IOUSBHostCIMessageType(
                        (msg.control & IOUSBHostCIMessageControlType)
                            >> IOUSBHostCIMessageControlTypePhase,
                    );
                    debug!("MSG type: {}", unsafe {
                        CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
                    });

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

                if msg.control & IOUSBHostCIMessageControlValid == 0 {
                    panic!("Message should be valid");
                }

                let data_length: u32 = (msg.data0 & IOUSBHostCINormalTransferData0Length)
                    >> IOUSBHostCINormalTransferData0LengthPhase;
                let buffer: &mut [u8] = unsafe {
                    from_raw_parts_mut(
                        ((msg.data1 & IOUSBHostCINormalTransferData1Buffer)
                            >> IOUSBHostCINormalTransferData1BufferPhase)
                            as *mut _,
                        data_length as usize,
                    )
                };

                debug!("Length: {}", data_length);
                debug!("Buffer: {:?}", buffer);

                let dir = if (endpoint_address & 0x00000080) != 0 {
                    UsbIpDirection::UsbDirIn
                } else {
                    UsbIpDirection::UsbDirOut
                };

                let ep = &ep.clone_state_machine();
                let ep_ptr = ForceableSend(ep.clone());
                let client = ForceableSend(client.clone());

                rt.spawn(async move {
                    let transfer_buffer = if dir == UsbIpDirection::UsbDirIn {
                        vec![]
                    } else {
                        buffer.to_vec()
                    };
                    let mut client = client;
                    let ret = unsafe {
                        client
                            .0
                            .borrow_mut()
                            .get()
                            .as_mut()
                            .unwrap()
                            .cmd_submit(
                                dir,
                                (endpoint_address & 0x0f) as u32,
                                0,
                                data_length,
                                0,
                                0,
                                0,
                                [0, 0, 0, 0, 0, 0, 0, 0],
                                &transfer_buffer,
                            )
                            .await
                    }
                    .unwrap();
                    run_on_main(move |_| {
                        debug!("Submit result: {:?}", ret);

                        let real_length = min(ret.buffer.len(), data_length as usize);
                        if (real_length as u32) != data_length {
                            debug!(
                                "WARN: Data length mismatch: {} != {}",
                                real_length, data_length
                            );
                        }

                        if dir == UsbIpDirection::UsbDirIn {
                            debug!("Copying data to buffer...");
                            buffer[..real_length]
                                .copy_from_slice(&ret.buffer[..real_length as usize]);
                        } else {
                            debug!("Copying data from buffer... NOT IMPLEMENTED");
                        }

                        let ep_ptr = ep_ptr;
                        let ep = ep_ptr.0;

                        let res = unsafe {
                            ep.enqueueTransferCompletionForMessage_status_transferLength_error(
                                NonNull::from(msg),
                                IOUSBHostCIMessageStatus::Success,
                                data_length as usize,
                            )
                        };
                        if res.is_err() {
                            panic!("Error: {:?}", res.err().unwrap());
                        }

                        let msg = unsafe { ep.currentTransferMessage().as_ref() };

                        let msg_type = IOUSBHostCIMessageType(
                            (msg.control & IOUSBHostCIMessageControlType)
                                >> IOUSBHostCIMessageControlTypePhase,
                        );
                        debug!("MSG type: {}", unsafe {
                            CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
                        });

                        if msg_type != IOUSBHostCIMessageType::Link {
                            panic!("Message is not a link");
                        }

                        if msg.control & IOUSBHostCIMessageControlValid != 0 {
                            // panic!("Message should be valid");
                            debug!("Message is valid");
                            return;
                        }

                        if msg.control & IOUSBHostCIMessageControlNoResponse == 0 {
                            let res = unsafe {
                                ep.enqueueTransferCompletionForMessage_status_transferLength_error(
                                    NonNull::from(msg),
                                    IOUSBHostCIMessageStatus::Success,
                                    0,
                                )
                            };
                            if res.is_err() {
                                panic!("Error: {:?}", res.err().unwrap());
                            }

                            let msg = unsafe { ep.currentTransferMessage().as_ref() };

                            let msg_type = IOUSBHostCIMessageType(
                                (msg.control & IOUSBHostCIMessageControlType)
                                    >> IOUSBHostCIMessageControlTypePhase,
                            );
                            debug!("MSG type: {}", unsafe {
                                CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
                            });

                            if msg.control & IOUSBHostCIMessageControlValid != 0 {
                                debug!("Message is valid");
                                return;
                            }
                        }
                    });
                });
            }
            IOUSBHostCIMessageType::Link => {
                warn!("Link message received"); // not usually expected here
                if msg.control & IOUSBHostCIMessageControlValid != 0 {
                    // panic!("Message should be valid");
                    debug!("Message is valid");
                    return;
                }

                if msg.control & IOUSBHostCIMessageControlNoResponse == 0 {
                    debug!("Needs response");
                    let ep = &ep.clone_state_machine();

                    let res = unsafe {
                        ep.enqueueTransferCompletionForMessage_status_transferLength_error(
                            NonNull::from(msg),
                            IOUSBHostCIMessageStatus::Success,
                            0,
                        )
                    };
                    if res.is_err() {
                        panic!("Error: {:?}", res.err().unwrap());
                    }

                    let msg = unsafe { ep.currentTransferMessage().as_ref() };

                    let msg_type = IOUSBHostCIMessageType(
                        (msg.control & IOUSBHostCIMessageControlType)
                            >> IOUSBHostCIMessageControlTypePhase,
                    );
                    debug!("MSG type: {}", unsafe {
                        CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
                    });

                    if msg.control & IOUSBHostCIMessageControlValid != 0 {
                        debug!("Message is valid");
                        return;
                    }
                }
            }
            _ => {
                panic!("Unexpected message type {}", unsafe {
                    CStr::from_ptr(msg_type.to_string()).to_str().unwrap()
                });
            }
        }
    }
}

unsafe extern "C-unwind" fn interest_handler(
    ref_con: *mut c_void,
    io_service_t: u32,
    message_type: u32,
    message_argument: *mut c_void,
) {
    debug!("Interest handler called with ref_con: {:?}, io_service_t: {}, message_type: {}, message_argument: {:?}", ref_con, io_service_t, message_type, message_argument);
}
