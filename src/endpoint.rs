use objc2::rc::Retained;
use objc2_io_usb_host::IOUSBHostCIEndpointStateMachine;

#[derive(Debug)]
pub struct Endpoint {
    state_machine: Retained<IOUSBHostCIEndpointStateMachine>,
}

impl Endpoint {
    pub fn new(state_machine: Retained<IOUSBHostCIEndpointStateMachine>) -> Self {
        Self { state_machine }
    }

    pub fn endpoint_address(&self) -> usize {
        unsafe { self.state_machine.endpointAddress() }
    }

    pub fn clone_state_machine(&self) -> Retained<IOUSBHostCIEndpointStateMachine> {
        self.state_machine.clone()
    }
}
