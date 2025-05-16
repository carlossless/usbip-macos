use core::panic;

use objc2::rc::Retained;
use objc2_io_usb_host::IOUSBHostCIDeviceStateMachine;

use crate::endpoint::Endpoint;

pub struct Device {
    state_machine: Retained<IOUSBHostCIDeviceStateMachine>,
    endpoints: Vec<Endpoint>,
}

impl Device {
    pub fn new(state_machine: Retained<IOUSBHostCIDeviceStateMachine>) -> Self {
        Device {
            state_machine,
            endpoints: Vec::new(),
        }
    }

    pub fn add_endpoint(&mut self, endpoint: Endpoint) {
        self.endpoints.push(endpoint);
    }

    pub fn get_device_address(&self) -> usize {
        unsafe { self.state_machine.deviceAddress() }
    }

    pub fn get_endpoints(&self) -> &Vec<Endpoint> {
        &self.endpoints
    }

    pub fn get_endpoint_by_addr(&self) -> &Vec<Endpoint> {
        panic!("Not implemented");
    }
}
