//! Moves transfers between host controller endpoint rings and a USB/IP session.
//!
//! Everything here except the network wait runs on one serial executor (the controller's
//! dispatch queue on macOS). A ring is only touched from that executor.
//!
//! Bulk and isochronous endpoints keep several URBs in flight by reading ahead in the ring, so
//! throughput is not bound by the network round trip; control and interrupt endpoints keep one.
//! Each in-flight URB is tied to the ring message it was built from, and replies are applied
//! strictly in ring order, only while their message is the head of an active ring. While an
//! endpoint is paused, replies wait until it resumes (see `pause_endpoint` for which URBs a pause
//! unlinks). If the host moves the ring elsewhere, the URBs are unlinked and their replies
//! dropped, so a buffer the host has reclaimed is never written.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use log::{debug, trace, warn};

use crate::{
    client::{Session, SubmitRequest, TransferError, TransferResult},
    hci::{self, EndpointInfo, EndpointKind, Status, Transfer},
    protocol::{Direction, IsoPacketDescriptor, PORT_RESET_SETUP, URB_ISO_ASAP},
};

/// Linux allows a reset several seconds to settle, enumeration included.
const RESET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One endpoint's transfer ring as exposed by the host controller.
pub trait Ring: Send + 'static {
    /// Whether the host lets us process and complete messages right now.
    fn active(&mut self) -> bool;
    /// Identity of the head message (stable while it stays the head) and its contents, or
    /// `None` contents if the head is not a valid message.
    fn current(&mut self) -> (u64, Option<Transfer>);
    /// The valid transfer message `ahead` positions after the head, following links. `None`
    /// when the ring holds nothing that far ahead.
    fn peek(&mut self, ahead: usize) -> Option<(u64, Transfer)>;
    /// Reads the buffer of the message `ahead` positions after the head (for OUT transfers).
    fn read(&mut self, ahead: usize, length: usize) -> Vec<u8>;
    /// Writes `data` into the head message's buffer and completes it with `length` bytes.
    fn complete(&mut self, status: Status, data: &[u8], length: u32) -> Result<(), String>;
}

/// Runs closures one at a time, in submission order.
pub trait Executor: Send + Sync + 'static {
    fn execute(&self, work: Box<dyn FnOnce() + Send>);
}

pub type EndpointKey = (u8, u8);

/// URBs kept in flight per endpoint. Isochronous needs enough to cover the round trip at one
/// message per (micro)frame.
fn depth(kind: EndpointKind) -> usize {
    match kind {
        EndpointKind::Isochronous => 32,
        EndpointKind::Bulk => 8,
        EndpointKind::Control | EndpointKind::Interrupt => 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    ControlData,
    ControlStatus,
    Normal,
    Isochronous,
}

#[derive(Debug)]
struct InFlight {
    token: u64,
    message: u64,
    stage: Stage,
    direction: Direction,
    length: u32,
    seqnum: u32,
    /// Unlinked because the endpoint was paused; submitted again if the host resumes here.
    unlinking: bool,
    /// `None` while the URB is still on the server.
    result: Option<TransferResult>,
}

#[derive(Debug, Clone, Copy)]
struct Setup {
    bytes: [u8; 8],
    data_done: bool,
}

struct Endpoint<R> {
    ring: R,
    info: EndpointInfo,
    in_flight: VecDeque<InFlight>,
    setup: Option<Setup>,
    /// Original and filtered `wTotalLength` once `hci::hide_uas` has changed a configuration.
    hidden_config: Option<(u16, u16)>,
}

struct State<R> {
    endpoints: HashMap<EndpointKey, Endpoint<R>>,
    next_token: u64,
}

struct Inner<R> {
    session: Session,
    runtime: tokio::runtime::Handle,
    executor: Arc<dyn Executor>,
    state: Mutex<State<R>>,
    uas_warned: AtomicBool,
}

pub struct Engine<R: Ring> {
    inner: Arc<Inner<R>>,
}

impl<R: Ring> Clone for Engine<R> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<R: Ring> Engine<R> {
    pub fn new(
        session: Session,
        runtime: tokio::runtime::Handle,
        executor: Arc<dyn Executor>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                session,
                runtime,
                executor,
                state: Mutex::new(State {
                    endpoints: HashMap::new(),
                    next_token: 0,
                }),
                uas_warned: AtomicBool::new(false),
            }),
        }
    }

    pub fn session(&self) -> &Session {
        &self.inner.session
    }

    /// Resets the device on the server and waits for it to finish. Blocks, so it must not be
    /// called from the async runtime.
    pub fn reset_device(&self) -> Result<(), TransferError> {
        let mut req = SubmitRequest::new(0, Direction::Out, 0);
        req.setup = PORT_RESET_SETUP;
        let (seqnum, reply) = self.inner.session.submit(req)?;
        debug!("resetting the device with seqnum {seqnum}");
        let reply = self
            .inner
            .runtime
            .block_on(async { tokio::time::timeout(RESET_TIMEOUT, reply).await })
            .map_err(|_| TransferError::Disconnected("device reset timed out".into()))??;
        match reply.status {
            0 => Ok(()),
            status => Err(TransferError::Invalid(format!(
                "device reset failed: {status}"
            ))),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<R>> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unlink_all(&self, in_flight: impl IntoIterator<Item = InFlight>) {
        for f in in_flight {
            if f.result.is_none() {
                debug!("cancelling seqnum {}", f.seqnum);
                self.inner.session.unlink(f.seqnum);
            }
        }
    }

    pub fn add_endpoint(&self, key: EndpointKey, info: EndpointInfo, ring: R) {
        let old = self.lock().endpoints.insert(
            key,
            Endpoint {
                ring,
                info,
                in_flight: VecDeque::new(),
                setup: None,
                hidden_config: None,
            },
        );
        if let Some(old) = old {
            self.unlink_all(old.in_flight);
        }
    }

    pub fn update_endpoint(&self, key: EndpointKey, info: EndpointInfo) {
        if let Some(ep) = self.lock().endpoints.get_mut(&key) {
            ep.info = info;
        }
    }

    pub fn remove_endpoint(&self, key: EndpointKey) {
        let removed = self.lock().endpoints.remove(&key);
        if let Some(ep) = removed {
            self.unlink_all(ep.in_flight);
        }
    }

    pub fn remove_device(&self, device: u8) {
        let removed: Vec<_> = {
            let mut state = self.lock();
            let keys: Vec<_> = state
                .endpoints
                .keys()
                .filter(|k| k.0 == device)
                .copied()
                .collect();
            keys.iter()
                .filter_map(|k| state.endpoints.remove(k))
                .collect()
        };
        for ep in removed {
            self.unlink_all(ep.in_flight);
        }
    }

    /// Discards everything in progress, as after the host resets a halted endpoint.
    pub fn reset_endpoint(&self, key: EndpointKey) {
        let in_flight = {
            let mut state = self.lock();
            let Some(ep) = state.endpoints.get_mut(&key) else {
                return;
            };
            ep.setup = None;
            std::mem::take(&mut ep.in_flight)
        };
        self.unlink_all(in_flight);
    }

    /// Unlinks the reads in flight on a paused data endpoint. macOS cancels a transfer by pausing
    /// the endpoint and completing it itself, and a read left pending on the server would take
    /// the data meant for the next one. Unlinked reads are submitted again if the host resumes at
    /// their message; replies that win the race are delivered then. Control and OUT transfers
    /// keep running: macOS pauses those routinely, and repeating one would make the device act
    /// twice.
    pub fn pause_endpoint(&self, key: EndpointKey) {
        if key.1 & 0x80 == 0 || key.1 & 0x7f == 0 {
            return;
        }
        let mut state = self.lock();
        let Some(ep) = state.endpoints.get_mut(&key) else {
            return;
        };
        for f in ep.in_flight.iter_mut().filter(|f| f.result.is_none()) {
            if !f.unlinking {
                f.unlinking = true;
                debug!("pausing seqnum {}", f.seqnum);
                self.inner.session.unlink(f.seqnum);
            }
        }
    }

    /// Cancels the in-flight URBs if the host has moved the ring away from their messages.
    pub fn sync_endpoint(&self, key: EndpointKey) {
        let in_flight = {
            let mut state = self.lock();
            let Some(ep) = state.endpoints.get_mut(&key) else {
                return;
            };
            let head = ep.ring.current().0;
            if ep.in_flight.front().is_some_and(|f| f.message != head) {
                std::mem::take(&mut ep.in_flight)
            } else {
                VecDeque::new()
            }
        };
        self.unlink_all(in_flight);
    }

    pub fn has_endpoint(&self, key: EndpointKey) -> bool {
        self.lock().endpoints.contains_key(&key)
    }

    /// Seqnum of the oldest URB still waiting on the server.
    pub fn in_flight(&self, key: EndpointKey) -> Option<u32> {
        self.lock().endpoints.get(&key).and_then(|e| {
            e.in_flight
                .iter()
                .find(|f| f.result.is_none())
                .map(|f| f.seqnum)
        })
    }

    pub fn in_flight_count(&self, key: EndpointKey) -> usize {
        self.lock()
            .endpoints
            .get(&key)
            .map_or(0, |e| e.in_flight.len())
    }

    pub fn doorbell(&self, key: EndpointKey) {
        let mut state = self.lock();
        self.pump(&mut state, key);
    }

    fn pump(&self, state: &mut State<R>, key: EndpointKey) {
        // Once the connection is gone, finished URBs are still delivered but nothing new is sent.
        let mut offline = false;
        loop {
            let Some(ep) = state.endpoints.get_mut(&key) else {
                return;
            };
            if !ep.ring.active() {
                return;
            }
            let (head, transfer) = ep.ring.current();
            if let Some(front) = ep.in_flight.front() {
                if front.message != head {
                    let stale = std::mem::take(&mut ep.in_flight);
                    self.unlink_all(stale);
                    continue;
                }
                if front.result.is_some() {
                    let front = ep.in_flight.pop_front().expect("checked above");
                    match apply(ep, front, &self.inner.uas_warned) {
                        Flow::Continue => continue,
                        Flow::Offline => {
                            offline = true;
                            continue;
                        }
                        Flow::Stop => return,
                    }
                }
                if !offline {
                    self.fill(state, key);
                }
                return;
            }
            let Some(transfer) = transfer else {
                return;
            };
            if offline {
                return;
            }
            match self.head_request(ep, key, transfer) {
                Next::Continue => continue,
                Next::Stop => return,
                Next::Submit(req, stage) => {
                    if !self.submit(state, key, head, req, stage) {
                        return;
                    }
                    self.fill(state, key);
                    return;
                }
            }
        }
    }

    /// Handles the head message when nothing is in flight.
    fn head_request(&self, ep: &mut Endpoint<R>, key: EndpointKey, transfer: Transfer) -> Next {
        let control = key.1 & 0x7f == 0;
        let continue_if = |ok: bool| if ok { Next::Continue } else { Next::Stop };
        match (transfer, control) {
            (Transfer::Setup(bytes), true) => {
                ep.setup = Some(Setup {
                    bytes,
                    data_done: false,
                });
                continue_if(complete(ep, Status::Success, &[], 0))
            }
            (Transfer::Normal { length }, true) => match ep.setup {
                Some(setup) if !setup.data_done => {
                    let w_length = u16::from_le_bytes([setup.bytes[6], setup.bytes[7]]);
                    let length = length.min(u32::from(w_length));
                    let req = control_request(ep, setup.bytes, length);
                    Next::Submit(req, Stage::ControlData)
                }
                _ => {
                    warn!("endpoint {key:?}: data stage without setup");
                    continue_if(complete(ep, Status::Error, &[], 0))
                }
            },
            (Transfer::Status, true) => match ep.setup {
                Some(setup) if !setup.data_done => {
                    Next::Submit(control_request(ep, setup.bytes, 0), Stage::ControlStatus)
                }
                _ => {
                    ep.setup = None;
                    continue_if(complete(ep, Status::Success, &[], 0))
                }
            },
            (Transfer::Link { needs_response }, _) => {
                if needs_response {
                    continue_if(complete(ep, Status::Success, &[], 0))
                } else {
                    Next::Stop
                }
            }
            (t @ (Transfer::Normal { .. } | Transfer::Isochronous { .. }), false) => {
                match self.data_request(ep, key, 0, t) {
                    Some((req, stage)) => Next::Submit(req, stage),
                    None => Next::Stop,
                }
            }
            (other, _) => {
                warn!("endpoint {key:?}: unexpected {other:?}");
                continue_if(complete(ep, Status::Error, &[], 0))
            }
        }
    }

    /// A URB for a bulk, interrupt or isochronous message `ahead` positions after the head.
    fn data_request(
        &self,
        ep: &mut Endpoint<R>,
        key: EndpointKey,
        ahead: usize,
        transfer: Transfer,
    ) -> Option<(SubmitRequest, Stage)> {
        let direction = Direction::of_endpoint(key.1);
        let interval = ep.info.urb_interval(self.inner.session.device().speed);
        match transfer {
            Transfer::Normal { length } => {
                let mut req = SubmitRequest::new(key.1, direction, length);
                req.interval = interval;
                if direction == Direction::Out {
                    req.data = ep.ring.read(ahead, length as usize);
                }
                Some((req, Stage::Normal))
            }
            Transfer::Isochronous { length, frame } => {
                let mut req = SubmitRequest::new(key.1, direction, length);
                if direction == Direction::Out {
                    req.data = ep.ring.read(ahead, length as usize);
                }
                match frame {
                    Some(frame) => req.start_frame = u32::from(frame),
                    None => req.transfer_flags |= URB_ISO_ASAP,
                }
                req.interval = interval.max(1);
                req.iso_packets = vec![IsoPacketDescriptor {
                    offset: 0,
                    length,
                    actual_length: 0,
                    status: 0,
                }];
                Some((req, Stage::Isochronous))
            }
            _ => None,
        }
    }

    /// Submits URBs for messages beyond those already in flight, up to the endpoint's depth.
    fn fill(&self, state: &mut State<R>, key: EndpointKey) {
        loop {
            let Some(ep) = state.endpoints.get_mut(&key) else {
                return;
            };
            let ahead = ep.in_flight.len();
            if ahead == 0
                || ahead >= depth(ep.info.kind)
                || ep.in_flight.iter().any(|f| f.result.is_some())
            {
                return;
            }
            let Some((message, transfer)) = ep.ring.peek(ahead) else {
                return;
            };
            let Some((req, stage)) = self.data_request(ep, key, ahead, transfer) else {
                return;
            };
            if !self.submit(state, key, message, req, stage) {
                return;
            }
        }
    }

    /// Returns whether a URB is now in flight for `message`.
    fn submit(
        &self,
        state: &mut State<R>,
        key: EndpointKey,
        message: u64,
        req: SubmitRequest,
        stage: Stage,
    ) -> bool {
        let token = state.next_token;
        let direction = req.direction;
        let length = req.length;
        trace!(
            "endpoint {key:?}: submit {stage:?} {direction:?} len {length} setup {:02x?} data {:02x?}",
            req.setup,
            &req.data[..req.data.len().min(32)]
        );
        let ep = state.endpoints.get_mut(&key).expect("endpoint vanished");
        match self.inner.session.submit(req) {
            Ok((seqnum, reply)) => {
                state.next_token += 1;
                ep.in_flight.push_back(InFlight {
                    token,
                    message,
                    stage,
                    direction,
                    length,
                    seqnum,
                    unlinking: false,
                    result: None,
                });
                let engine = self.clone();
                self.inner.runtime.spawn(async move {
                    let result = reply.await;
                    let executor = Arc::clone(&engine.inner.executor);
                    executor.execute(Box::new(move || engine.finish(key, token, result)));
                });
                true
            }
            Err(e) => {
                // Only the head can be completed; a failure further ahead is retried later.
                if ep.in_flight.is_empty() {
                    let status = error_status(&e);
                    if complete(ep, status, &[], 0) && status != Status::Offline {
                        self.pump(state, key);
                    }
                }
                false
            }
        }
    }

    fn finish(&self, key: EndpointKey, token: u64, result: TransferResult) {
        let mut state = self.lock();
        let Some(ep) = state.endpoints.get_mut(&key) else {
            return;
        };
        let Some(index) = ep.in_flight.iter().position(|f| f.token == token) else {
            debug!("endpoint {key:?}: dropping stale completion");
            return;
        };
        if ep.in_flight[index].unlinking && matches!(result, Err(TransferError::Unlinked(_))) {
            // Later reads may have run ahead of this one; all of them start again from here.
            let mut stale = ep.in_flight.split_off(index);
            stale.pop_front();
            self.unlink_all(stale);
        } else {
            ep.in_flight[index].result = Some(result);
        }
        self.pump(&mut state, key);
    }
}

enum Flow {
    Continue,
    Stop,
    Offline,
}

enum Next {
    Continue,
    Stop,
    Submit(SubmitRequest, Stage),
}

/// Completes the head message with a URB result.
fn apply<R: Ring>(ep: &mut Endpoint<R>, flight: InFlight, uas_warned: &AtomicBool) -> Flow {
    let result = flight.result.expect("only finished transfers are applied");
    let (status, data, length) = match result {
        Ok(ret) => {
            let packet = ret.iso_packets.first().copied();
            let (status, actual) = match (flight.stage, packet) {
                // Linux reports isochronous results per packet; the URB total may be 0.
                (Stage::Isochronous, Some(p)) => {
                    (Status::from_urb_status(p.status), p.actual_length)
                }
                _ => (Status::from_urb_status(ret.status), ret.actual_length),
            };
            let length = actual.min(flight.length);
            let mut data = ret.data;
            data.truncate(length as usize);
            (status, data, length)
        }
        Err(e) => (error_status(&e), Vec::new(), 0),
    };
    let mut data = if flight.direction == Direction::In {
        data
    } else {
        Vec::new()
    };
    let mut length = length;
    match flight.stage {
        Stage::ControlData => {
            if let Some(setup) = ep.setup.as_mut() {
                setup.data_done = true;
            }
            // GET_DESCRIPTOR(CONFIGURATION)
            if ep
                .setup
                .is_some_and(|s| s.bytes[..2] == [0x80, 0x06] && s.bytes[3] == 2)
            {
                if let Some(totals) = hci::hide_uas(&mut data) {
                    if !uas_warned.swap(true, Ordering::Relaxed) {
                        warn!(
                            "the device supports USB Attached SCSI, which USB/IP cannot carry; \
                             using its slower Bulk-Only Transport mode instead"
                        );
                    }
                    ep.hidden_config = Some(totals);
                } else if let Some((original, hidden)) = ep.hidden_config {
                    // A read too short to reach the interfaces must still report the length
                    // the full read returns, or macOS gives up on the device.
                    if data.len() >= 4 && data[2..4] == original.to_le_bytes() {
                        data[2..4].copy_from_slice(&hidden.to_le_bytes());
                    }
                }
                length = length.min(data.len() as u32);
            }
        }
        Stage::ControlStatus => ep.setup = None,
        Stage::Normal | Stage::Isochronous => {}
    }
    trace!(
        "{:?} {:?} completed {status:?} len {length} data {:02x?}",
        flight.stage,
        flight.direction,
        &data[..data.len().min(32)]
    );
    match (complete(ep, status, &data, length), status) {
        (false, _) => Flow::Stop,
        (true, Status::Offline) => Flow::Offline,
        (true, _) => Flow::Continue,
    }
}

fn control_request(ep: &mut Endpoint<impl Ring>, setup: [u8; 8], length: u32) -> SubmitRequest {
    let direction = if setup[0] & 0x80 != 0 {
        Direction::In
    } else {
        Direction::Out
    };
    let mut req = SubmitRequest::new(0, direction, length);
    req.setup = setup;
    if direction == Direction::Out {
        req.data = ep.ring.read(0, length as usize);
    }
    req
}

fn complete<R: Ring>(ep: &mut Endpoint<R>, status: Status, data: &[u8], length: u32) -> bool {
    match ep.ring.complete(status, data, length) {
        Ok(()) => true,
        Err(e) => {
            debug!("completion rejected: {e}");
            false
        }
    }
}

fn error_status(e: &TransferError) -> Status {
    match e {
        TransferError::Disconnected(_) => Status::Offline,
        TransferError::Unlinked(_) => Status::EndpointStopped,
        TransferError::Invalid(_) => Status::Error,
    }
}
