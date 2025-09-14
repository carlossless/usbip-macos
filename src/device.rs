use objc2::rc::Retained;
use objc2_io_usb_host::IOUSBHostCIDeviceStateMachine;

use crate::endpoint::Endpoint;

#[derive(Debug)]
pub struct Device {
    state_machine: Retained<IOUSBHostCIDeviceStateMachine>,
    endpoints: Vec<Endpoint>,
}

impl Device {
    pub fn new(state_machine: Retained<IOUSBHostCIDeviceStateMachine>) -> Self {
        Self {
            state_machine,
            endpoints: Vec::new(),
        }
    }

    pub fn add_endpoint(&mut self, endpoint: Endpoint) {
        self.endpoints.push(endpoint);
    }

    pub fn device_address(&self) -> usize {
        unsafe { self.state_machine.deviceAddress() }
    }

    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }
}
