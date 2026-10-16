//! Virtual USB host controller backed by `IOUSBHostControllerInterface`.
//!
//! Every handler runs on one serial dispatch queue that we own, and every state machine call
//! is made from that queue. The objects below are therefore never used concurrently, which is
//! what the `unsafe impl Send` declarations rely on.

use std::{
    collections::HashMap,
    ffi::c_void,
    mem,
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use log::{debug, error, warn};
use objc2::{rc::Retained, AnyThread};
use objc2_foundation::{NSError, NSMutableData};
use objc2_io_usb_host::{
    IOUSBHostCICapabilitiesMessageControlPortCountPhase,
    IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase,
    IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase, IOUSBHostCIControllerState,
    IOUSBHostCIDeviceSpeed, IOUSBHostCIDeviceStateMachine, IOUSBHostCIDoorbell,
    IOUSBHostCIEndpointState, IOUSBHostCIEndpointStateMachine, IOUSBHostCILinkState,
    IOUSBHostCIMessage, IOUSBHostCIMessageControlNoResponse, IOUSBHostCIMessageControlTypePhase,
    IOUSBHostCIMessageControlValid, IOUSBHostCIMessageStatus, IOUSBHostCIMessageType,
    IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase,
    IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase, IOUSBHostCIPortStateMachine,
    IOUSBHostControllerInterface,
};
use tokio::sync::watch;

use crate::{
    client::Session,
    engine::{EndpointKey, Engine, Executor, Ring},
    hci::{self, message_type as mt, AddressPool, EndpointInfo, RawMessage, Status, Transfer},
    protocol::Speed,
};

const COMMAND_TIMEOUT_THRESHOLD: u32 = 1;
const CONNECTION_LATENCY: u32 = 2;
const MAX_POWER_MA: u32 = 900;
const ROOT_PORT: usize = 1;
const FRAME_UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebase) -> i32;
}

/// Maps `mach_absolute_time` onto the 1 ms USB frames the kernel schedules isochronous
/// transfers against. The kernel shuts the controller down if two reports carry the same frame.
struct FrameClock {
    epoch: u64,
    numer: u128,
    denom: u128,
    last: Mutex<Option<u64>>,
}

impl FrameClock {
    fn new() -> Self {
        let mut tb = MachTimebase { numer: 1, denom: 1 };
        unsafe { mach_timebase_info(&mut tb) };
        Self {
            epoch: unsafe { mach_absolute_time() },
            numer: u128::from(tb.numer),
            denom: u128::from(tb.denom),
            last: Mutex::new(None),
        }
    }

    fn frame_at(&self, ticks: u64) -> u64 {
        (u128::from(ticks - self.epoch) * self.numer / (self.denom * 1_000_000)) as u64
    }

    fn start_of(&self, frame: u64) -> u64 {
        self.epoch + (u128::from(frame) * self.denom * 1_000_000 / self.numer) as u64
    }

    /// A frame number later than any reported before, and the mach time at which it began.
    fn next(&self) -> (u64, u64) {
        let mut last = lock(&self.last);
        let mut frame = self.frame_at(unsafe { mach_absolute_time() });
        while last.is_some_and(|l| frame <= l) {
            std::thread::sleep(Duration::from_micros(200));
            frame = self.frame_at(unsafe { mach_absolute_time() });
        }
        *last = Some(frame);
        (frame, self.start_of(frame))
    }
}

fn raw(msg: &IOUSBHostCIMessage) -> RawMessage {
    RawMessage {
        control: msg.control,
        data0: msg.data0,
        data1: msg.data1,
    }
}

fn ci_status(status: Status) -> IOUSBHostCIMessageStatus {
    match status {
        Status::Success => IOUSBHostCIMessageStatus::Success,
        Status::Offline => IOUSBHostCIMessageStatus::Offline,
        Status::Timeout => IOUSBHostCIMessageStatus::Timeout,
        Status::EndpointStopped => IOUSBHostCIMessageStatus::EndpointStopped,
        Status::ProtocolError => IOUSBHostCIMessageStatus::ProtocolError,
        Status::TransactionError => IOUSBHostCIMessageStatus::TransactionError,
        Status::OverrunError => IOUSBHostCIMessageStatus::OverrunError,
        Status::StallError => IOUSBHostCIMessageStatus::StallError,
        Status::Error => IOUSBHostCIMessageStatus::Error,
    }
}

fn ci_speed(speed: Speed) -> IOUSBHostCIDeviceSpeed {
    match speed {
        Speed::Low => IOUSBHostCIDeviceSpeed::Low,
        Speed::Full | Speed::Unknown => IOUSBHostCIDeviceSpeed::Full,
        Speed::High | Speed::Wireless => IOUSBHostCIDeviceSpeed::High,
        Speed::Super => IOUSBHostCIDeviceSpeed::Super,
        Speed::SuperPlus => IOUSBHostCIDeviceSpeed::SuperPlus,
    }
}

fn describe(err: Retained<NSError>) -> String {
    format!("{} ({:#x})", err.localizedDescription(), err.code() as u32)
}

struct QueueExecutor(DispatchRetained<DispatchQueue>);

impl Executor for QueueExecutor {
    fn execute(&self, work: Box<dyn FnOnce() + Send>) {
        self.0.exec_async(work);
    }
}

pub struct IoRing(Retained<IOUSBHostCIEndpointStateMachine>);

// SAFETY: only used from the controller queue, see the module documentation.
unsafe impl Send for IoRing {}

impl IoRing {
    fn message(&self) -> NonNull<IOUSBHostCIMessage> {
        unsafe { self.0.currentTransferMessage() }
    }

    /// The valid transfer message `ahead` positions after the head. Messages sit in arrays
    /// joined by link messages; the host marks each one valid once it owns nothing in it.
    fn message_at(&self, ahead: usize) -> Option<NonNull<IOUSBHostCIMessage>> {
        let mut ptr = self.message();
        let mut remaining = ahead;
        // Bounds the walk should a malformed ring link back on itself.
        for _ in 0..(ahead + 1) * 4 {
            let msg = raw(unsafe { ptr.as_ref() });
            if !msg.is_valid() {
                return None;
            }
            if msg.message_type() == hci::message_type::LINK {
                ptr = NonNull::new(msg.data1 as *mut IOUSBHostCIMessage)?;
                continue;
            }
            if remaining == 0 {
                return Some(ptr);
            }
            remaining -= 1;
            // SAFETY: a valid non-link message is followed by another message of the same array.
            ptr = unsafe { ptr.add(1) };
        }
        None
    }
}

impl Ring for IoRing {
    fn active(&mut self) -> bool {
        (unsafe { self.0.endpointState() }) == IOUSBHostCIEndpointState::Active
    }

    fn current(&mut self) -> (u64, Option<Transfer>) {
        let ptr = self.message();
        (
            ptr.as_ptr() as u64,
            Transfer::decode(&raw(unsafe { ptr.as_ref() })),
        )
    }

    fn peek(&mut self, ahead: usize) -> Option<(u64, Transfer)> {
        let ptr = self.message_at(ahead)?;
        let transfer = Transfer::decode(&raw(unsafe { ptr.as_ref() }))?;
        Some((ptr.as_ptr() as u64, transfer))
    }

    fn read(&mut self, ahead: usize, length: usize) -> Vec<u8> {
        let Some(ptr) = self.message_at(ahead) else {
            return Vec::new();
        };
        let msg = raw(unsafe { ptr.as_ref() });
        let length = length.min(msg.transfer_length() as usize);
        if length == 0 {
            return Vec::new();
        }
        // SAFETY: the host keeps a valid message's buffer mapped until it is completed.
        unsafe { std::slice::from_raw_parts(msg.buffer() as *const u8, length) }.to_vec()
    }

    fn complete(&mut self, status: Status, data: &[u8], length: u32) -> Result<(), String> {
        let ptr = self.message();
        let msg = raw(unsafe { ptr.as_ref() });
        let n = data.len().min(msg.transfer_length() as usize);
        if n > 0 {
            // SAFETY: as in `read`; `n` is bounded by the buffer length the host gave us.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), msg.buffer() as *mut u8, n);
            }
        }
        unsafe {
            self.0
                .enqueueTransferCompletionForMessage_status_transferLength_error(
                    ptr,
                    ci_status(status),
                    length.min(msg.transfer_length()) as usize,
                )
        }
        .map_err(describe)
    }
}

struct Shared {
    engine: Engine<IoRing>,
    speed: IOUSBHostCIDeviceSpeed,
    clock: FrameClock,
    devices: Mutex<HashMap<u8, Retained<IOUSBHostCIDeviceStateMachine>>>,
    endpoints: Mutex<HashMap<EndpointKey, Retained<IOUSBHostCIEndpointStateMachine>>>,
    addresses: Mutex<AddressPool>,
    powered: watch::Sender<bool>,
    /// Set once macOS has created a device; port resets before that are the initial one.
    enumerated: AtomicBool,
}

// SAFETY: the state machines are only used from the controller queue.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Controller {
    interface: Retained<IOUSBHostControllerInterface>,
    queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    frames: tokio::task::JoinHandle<()>,
}

// SAFETY: `interface` is only touched through `queue`, except for `destroy`, which is
// documented as callable at any time.
unsafe impl Send for Controller {}
unsafe impl Sync for Controller {}

impl Controller {
    pub fn new(session: Session, runtime: tokio::runtime::Handle) -> Result<Self, String> {
        let queue = DispatchQueue::new("usbip.controller", None);
        let speed = ci_speed(session.device().speed);
        let engine = Engine::new(
            session,
            runtime.clone(),
            Arc::new(QueueExecutor(queue.clone())),
        );
        let shared = Arc::new(Shared {
            engine,
            speed,
            clock: FrameClock::new(),
            devices: Mutex::default(),
            endpoints: Mutex::default(),
            addresses: Mutex::default(),
            powered: watch::channel(false).0,
            enumerated: AtomicBool::new(false),
        });

        let command_shared = Arc::clone(&shared);
        let command_block = RcBlock::new(
            move |controller: NonNull<IOUSBHostControllerInterface>,
                  command: IOUSBHostCIMessage| {
                let controller = unsafe { controller.as_ref() };
                if let Err(e) = command_shared.command(controller, &command) {
                    error!("command {:#04x}: {e}", raw(&command).message_type());
                }
            },
        );
        let doorbell_shared = Arc::clone(&shared);
        let doorbell_block = RcBlock::new(
            move |_: NonNull<IOUSBHostControllerInterface>,
                  doorbells: NonNull<IOUSBHostCIDoorbell>,
                  count: u32| {
                let doorbells =
                    unsafe { std::slice::from_raw_parts(doorbells.as_ptr(), count as usize) };
                for &doorbell in doorbells {
                    doorbell_shared.doorbell(doorbell);
                }
            },
        );

        let capabilities = capabilities();
        let mut err = None;
        let interface = unsafe {
            IOUSBHostControllerInterface::initWithCapabilities_queue_interruptRateHz_error_commandHandler_doorbellHandler_interestHandler(
                IOUSBHostControllerInterface::alloc(),
                &capabilities,
                Some(&queue),
                1000,
                Some(&mut err),
                RcBlock::as_ptr(&command_block),
                RcBlock::as_ptr(&doorbell_block),
                Some(interest_handler),
            )
        };
        let interface = interface.ok_or_else(|| match err {
            Some(err) => format!("failed to create controller interface: {}", describe(err)),
            None => "failed to create controller interface".to_string(),
        })?;
        let frames = spawn_frame_updates(&runtime, &queue, &interface, &shared);
        Ok(Self {
            interface,
            queue,
            shared,
            frames,
        })
    }

    pub async fn wait_powered(&self) {
        let _ = self.shared.powered.subscribe().wait_for(|p| *p).await;
    }

    fn with_port(&self, f: impl FnOnce(&IOUSBHostCIPortStateMachine) + Send) {
        let interface = ForceSend(self.interface.clone());
        self.queue.exec_sync(move || {
            let interface = interface;
            match unsafe { interface.0.getPortStateMachineForPort_error(ROOT_PORT) } {
                Ok(port) => f(&port),
                Err(e) => error!("root port unavailable: {}", describe(e)),
            }
        });
    }

    pub fn set_connected(&self, connected: bool) {
        self.with_port(|port| unsafe { port.setConnected(connected) });
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.frames.abort();
        let interface = ForceSend(self.interface.clone());
        self.queue.exec_sync(move || {
            let interface = interface;
            unsafe { interface.0.destroy() };
        });
    }
}

struct ForceSend<T>(T);
// SAFETY: used only to move the interface onto its own queue.
unsafe impl<T> Send for ForceSend<T> {}
unsafe impl<T> Sync for ForceSend<T> {}

/// Keeps the kernel's view of the frame counter fresh so isochronous requests can be scheduled
/// without waiting for it to ask.
fn spawn_frame_updates(
    runtime: &tokio::runtime::Handle,
    queue: &DispatchRetained<DispatchQueue>,
    interface: &Retained<IOUSBHostControllerInterface>,
    shared: &Arc<Shared>,
) -> tokio::task::JoinHandle<()> {
    let queue = queue.clone();
    let interface = ForceSend(interface.clone());
    let shared = Arc::clone(shared);
    runtime.spawn(async move {
        let interface = interface;
        let mut tick = tokio::time::interval(FRAME_UPDATE_INTERVAL);
        loop {
            tick.tick().await;
            let interface = ForceSend(interface.0.clone());
            let shared = Arc::clone(&shared);
            queue.exec_async(move || {
                let interface = interface;
                let sm = unsafe { interface.0.controllerStateMachine() };
                if unsafe { sm.controllerState() } != IOUSBHostCIControllerState::Active {
                    return;
                }
                let (frame, time) = shared.clock.next();
                if let Err(e) = unsafe { sm.enqueueUpdatedFrame_timestamp_error(frame, time) } {
                    debug!("frame update {frame}: {}", describe(e));
                }
            });
        }
    })
}

fn capabilities() -> Retained<NSMutableData> {
    let controller = IOUSBHostCIMessage {
        control: (IOUSBHostCIMessageType::ControllerCapabilities.0
            << IOUSBHostCIMessageControlTypePhase)
            | IOUSBHostCIMessageControlNoResponse
            | IOUSBHostCIMessageControlValid
            | (1 << IOUSBHostCICapabilitiesMessageControlPortCountPhase),
        data0: (COMMAND_TIMEOUT_THRESHOLD
            << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase)
            | (CONNECTION_LATENCY << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),
        data1: 0,
    };
    let port = IOUSBHostCIMessage {
        control: (IOUSBHostCIMessageType::PortCapabilities.0 << IOUSBHostCIMessageControlTypePhase)
            | IOUSBHostCIMessageControlNoResponse
            | IOUSBHostCIMessageControlValid
            | ((ROOT_PORT as u32) << IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase),
        data0: (MAX_POWER_MA / 8) << IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase,
        data1: 0,
    };
    let data = NSMutableData::new();
    for msg in [controller, port] {
        unsafe {
            data.appendBytes_length(
                NonNull::from(&msg).cast::<c_void>(),
                mem::size_of::<IOUSBHostCIMessage>(),
            )
        };
    }
    data
}

/// Reads the endpoint descriptor an EndpointCreate or EndpointUpdate command points at.
fn endpoint_descriptor(msg: &RawMessage) -> EndpointInfo {
    if msg.data1 == 0 {
        return EndpointInfo::CONTROL;
    }
    // SAFETY: the kernel passes a mapped descriptor that outlives the command; its own length
    // fields bound the read.
    let head = unsafe { std::slice::from_raw_parts(msg.data1 as *const u8, 4) };
    let len = if head[1] == 2 {
        u16::from_le_bytes([head[2], head[3]]).clamp(4, 512) as usize
    } else {
        7
    };
    let desc = unsafe { std::slice::from_raw_parts(msg.data1 as *const u8, len) };
    EndpointInfo::parse(desc).unwrap_or(EndpointInfo::CONTROL)
}

type CommandResult = Result<(), String>;

type NsResult = Result<(), Retained<NSError>>;

/// Inspects and answers a command. The host waits for an answer to every command, so one is
/// sent even when the state machine rejects the command or `act` fails.
fn answer(
    inspect: impl FnOnce() -> NsResult,
    respond: impl Fn(IOUSBHostCIMessageStatus) -> NsResult,
    act: impl FnOnce() -> CommandResult,
) -> CommandResult {
    if let Err(e) = inspect() {
        let _ = respond(IOUSBHostCIMessageStatus::Error);
        return Err(format!("rejected: {}", describe(e)));
    }
    let result = act();
    let status = match result {
        Ok(()) => IOUSBHostCIMessageStatus::Success,
        Err(_) => IOUSBHostCIMessageStatus::Error,
    };
    respond(status).map_err(describe)?;
    result
}

impl Shared {
    fn command(
        &self,
        controller: &IOUSBHostControllerInterface,
        command: &IOUSBHostCIMessage,
    ) -> CommandResult {
        let msg = raw(command);
        let ptr = NonNull::from(command);
        let kind = msg.message_type();
        debug!("command {kind:#04x} data0 {:#010x}", msg.data0);
        match kind {
            mt::CONTROLLER_POWER_ON..=mt::CONTROLLER_PAUSE => {
                let sm = unsafe { controller.controllerStateMachine() };
                answer(
                    || unsafe { sm.inspectCommand_error(ptr) },
                    |s| unsafe { sm.respondToCommand_status_error(ptr, s) },
                    || Ok(()),
                )
            }
            mt::CONTROLLER_FRAME_NUMBER => {
                let sm = unsafe { controller.controllerStateMachine() };
                let (frame, now) = self.clock.next();
                answer(
                    || unsafe { sm.inspectCommand_error(ptr) },
                    |s| unsafe {
                        sm.respondToCommand_status_frame_timestamp_error(ptr, s, frame, now)
                    },
                    || Ok(()),
                )
            }
            mt::PORT_POWER_ON..=mt::PORT_STATUS => {
                let port = unsafe { controller.getPortStateMachineForCommand_error(ptr) }
                    .map_err(describe)?;
                answer(
                    || unsafe { port.inspectCommand_error(ptr) },
                    |s| unsafe { port.respondToCommand_status_error(ptr, s) },
                    || match kind {
                        mt::PORT_RESET => {
                            if self.enumerated.load(Ordering::Relaxed) {
                                if let Err(e) = self.engine.reset_device() {
                                    warn!("device reset: {e}");
                                }
                            }
                            unsafe {
                                port.updateLinkState_speed_inhibitLinkStateChange_error(
                                    IOUSBHostCILinkState::U0,
                                    self.speed,
                                    false,
                                )
                            }
                            .map_err(describe)
                        }
                        _ => Ok(()),
                    },
                )?;
                if kind == mt::PORT_POWER_ON {
                    self.powered.send_replace(true);
                }
                Ok(())
            }
            mt::DEVICE_CREATE => {
                let sm = unsafe {
                    IOUSBHostCIDeviceStateMachine::initWithInterface_command_error(
                        IOUSBHostCIDeviceStateMachine::alloc(),
                        controller,
                        ptr,
                    )
                }
                .map_err(describe)?;
                let Some(address) = lock(&self.addresses).allocate() else {
                    let _ = unsafe {
                        sm.respondToCommand_status_error(ptr, IOUSBHostCIMessageStatus::NoResources)
                    };
                    return Err("out of device addresses".into());
                };
                if let Err(e) = unsafe {
                    sm.respondToCommand_status_deviceAddress_error(
                        ptr,
                        IOUSBHostCIMessageStatus::Success,
                        address as usize,
                    )
                } {
                    lock(&self.addresses).release(address);
                    return Err(describe(e));
                }
                self.enumerated.store(true, Ordering::Relaxed);
                debug!("created device {address}");
                lock(&self.devices).insert(address, sm);
                Ok(())
            }
            mt::DEVICE_DESTROY..=mt::DEVICE_UPDATE => {
                let address = msg.device_address();
                let sm = lock(&self.devices)
                    .get(&address)
                    .cloned()
                    .ok_or_else(|| format!("unknown device {address}"))?;
                answer(
                    || unsafe { sm.inspectCommand_error(ptr) },
                    |s| unsafe { sm.respondToCommand_status_error(ptr, s) },
                    || {
                        match kind {
                            mt::DEVICE_DESTROY => {
                                self.engine.remove_device(address);
                                lock(&self.endpoints).retain(|k, _| k.0 != address);
                                lock(&self.devices).remove(&address);
                                lock(&self.addresses).release(address);
                            }
                            mt::DEVICE_PAUSE => {
                                let keys: Vec<_> = lock(&self.endpoints)
                                    .keys()
                                    .filter(|k| k.0 == address)
                                    .copied()
                                    .collect();
                                for key in keys {
                                    self.engine.pause_endpoint(key);
                                }
                            }
                            _ => {}
                        }
                        Ok(())
                    },
                )
            }
            mt::ENDPOINT_CREATE => {
                let sm = unsafe {
                    IOUSBHostCIEndpointStateMachine::initWithInterface_command_error(
                        IOUSBHostCIEndpointStateMachine::alloc(),
                        controller,
                        ptr,
                    )
                }
                .map_err(describe)?;
                unsafe { sm.respondToCommand_status_error(ptr, IOUSBHostCIMessageStatus::Success) }
                    .map_err(describe)?;
                let key = unsafe { (sm.deviceAddress() as u8, sm.endpointAddress() as u8) };
                let info = endpoint_descriptor(&msg);
                debug!("created endpoint {key:?} {info:?}");
                lock(&self.endpoints).insert(key, sm.clone());
                self.engine.add_endpoint(key, info, IoRing(sm));
                Ok(())
            }
            mt::ENDPOINT_DESTROY..=mt::ENDPOINT_SET_NEXT_TRANSFER => {
                let key = (msg.device_address(), msg.endpoint_address());
                let sm = lock(&self.endpoints)
                    .get(&key)
                    .cloned()
                    .ok_or_else(|| format!("unknown endpoint {key:?}"))?;
                answer(
                    || unsafe { sm.inspectCommand_error(ptr) },
                    |s| unsafe { sm.respondToCommand_status_error(ptr, s) },
                    || {
                        match kind {
                            mt::ENDPOINT_DESTROY => {
                                self.engine.remove_endpoint(key);
                                lock(&self.endpoints).remove(&key);
                            }
                            mt::ENDPOINT_PAUSE => self.engine.pause_endpoint(key),
                            mt::ENDPOINT_UPDATE => {
                                self.engine.update_endpoint(key, endpoint_descriptor(&msg))
                            }
                            mt::ENDPOINT_RESET => self.engine.reset_endpoint(key),
                            _ => {}
                        }
                        Ok(())
                    },
                )?;
                if kind == mt::ENDPOINT_SET_NEXT_TRANSFER {
                    self.engine.sync_endpoint(key);
                }
                Ok(())
            }
            other => {
                warn!("ignoring unsupported command {other:#04x}");
                Ok(())
            }
        }
    }

    fn doorbell(&self, doorbell: IOUSBHostCIDoorbell) {
        let (device, endpoint, _stream) = hci::decode_doorbell(doorbell);
        let key = (device, endpoint);
        let Some(sm) = lock(&self.endpoints).get(&key).cloned() else {
            warn!("doorbell for unknown endpoint {key:?}");
            return;
        };
        if let Err(e) = unsafe { sm.processDoorbell_error(doorbell) } {
            error!("doorbell {key:?}: {}", describe(e));
            return;
        }
        self.engine.doorbell(key);
    }
}

unsafe extern "C-unwind" fn interest_handler(
    _ref_con: *mut c_void,
    _service: u32,
    message_type: u32,
    _argument: *mut c_void,
) {
    debug!("controller interest notification {message_type:#x}");
}

#[cfg(test)]
mod tests {
    use objc2_io_usb_host::*;

    use crate::hci::{self, message_type as mt};

    #[test]
    fn bit_layout_matches_sdk() {
        assert_eq!(hci::CONTROL_TYPE_MASK, IOUSBHostCIMessageControlType);
        assert_eq!(hci::CONTROL_STATUS_MASK, IOUSBHostCIMessageControlStatus);
        assert_eq!(
            hci::CONTROL_NO_RESPONSE,
            IOUSBHostCIMessageControlNoResponse
        );
        assert_eq!(hci::CONTROL_VALID, IOUSBHostCIMessageControlValid);
        assert_eq!(
            hci::ISO_CONTROL_ASAP,
            IOUSBHostCIIsochronousTransferControlASAP
        );
        assert_eq!(
            hci::ISO_CONTROL_FRAME_MASK,
            IOUSBHostCIIsochronousTransferControlFrameNumber
        );
        assert_eq!(
            hci::TRANSFER_LENGTH_MASK,
            IOUSBHostCINormalTransferData0Length
        );
        assert_eq!(0xff, IOUSBHostCICommandMessageData0DeviceAddress);
        assert_eq!(0xff00, IOUSBHostCICommandMessageData0EndpointAddress);
        assert_eq!(0xff, IOUSBHostCIDoorbellDeviceAddress);
        assert_eq!(0xff00, IOUSBHostCIDoorbellEndpointAddress);
        assert_eq!(0xffff_0000, IOUSBHostCIDoorbellStreamID);
    }

    #[test]
    fn message_types_match_sdk() {
        use IOUSBHostCIMessageType as T;
        let pairs = [
            (mt::CONTROLLER_POWER_ON, T::ControllerPowerOn),
            (mt::CONTROLLER_POWER_OFF, T::ControllerPowerOff),
            (mt::CONTROLLER_START, T::ControllerStart),
            (mt::CONTROLLER_PAUSE, T::ControllerPause),
            (mt::CONTROLLER_FRAME_NUMBER, T::ControllerFrameNumber),
            (mt::PORT_POWER_ON, T::PortPowerOn),
            (mt::PORT_POWER_OFF, T::PortPowerOff),
            (mt::PORT_RESUME, T::PortResume),
            (mt::PORT_SUSPEND, T::PortSuspend),
            (mt::PORT_RESET, T::PortReset),
            (mt::PORT_DISABLE, T::PortDisable),
            (mt::PORT_STATUS, T::PortStatus),
            (mt::DEVICE_CREATE, T::DeviceCreate),
            (mt::DEVICE_DESTROY, T::DeviceDestroy),
            (mt::DEVICE_START, T::DeviceStart),
            (mt::DEVICE_PAUSE, T::DevicePause),
            (mt::DEVICE_UPDATE, T::DeviceUpdate),
            (mt::ENDPOINT_CREATE, T::EndpointCreate),
            (mt::ENDPOINT_DESTROY, T::EndpointDestroy),
            (mt::ENDPOINT_PAUSE, T::EndpointPause),
            (mt::ENDPOINT_UPDATE, T::EndpointUpdate),
            (mt::ENDPOINT_RESET, T::EndpointReset),
            (mt::ENDPOINT_SET_NEXT_TRANSFER, T::EndpointSetNextTransfer),
            (mt::SETUP_TRANSFER, T::SetupTransfer),
            (mt::NORMAL_TRANSFER, T::NormalTransfer),
            (mt::STATUS_TRANSFER, T::StatusTransfer),
            (mt::ISOCHRONOUS_TRANSFER, T::IsochronousTransfer),
            (mt::LINK, T::Link),
        ];
        for (ours, sdk) in pairs {
            assert_eq!(ours, sdk.0, "{sdk:?}");
        }
    }
}
