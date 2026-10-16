//! A USB/IP server for tests. It parses the wire format independently of the client's codec
//! so encoding mistakes on either side show up as failures instead of cancelling out.

use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
};

pub const ECONNRESET: i32 = 104;
pub const EPIPE: i32 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Urb {
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
    pub transfer_flags: u32,
    pub length: u32,
    pub start_frame: u32,
    pub number_of_packets: u32,
    pub interval: u32,
    pub setup: [u8; 8],
    pub data: Vec<u8>,
    pub iso: Vec<IsoDesc>,
}

impl Urb {
    pub fn is_in(&self) -> bool {
        self.direction == 1
    }

    pub fn request_type(&self) -> u8 {
        self.setup[0]
    }

    pub fn request(&self) -> u8 {
        self.setup[1]
    }

    pub fn value(&self) -> u16 {
        u16::from_le_bytes([self.setup[2], self.setup[3]])
    }

    pub fn index(&self) -> u16 {
        u16::from_le_bytes([self.setup[4], self.setup[5]])
    }

    pub fn w_length(&self) -> u16 {
        u16::from_le_bytes([self.setup[6], self.setup[7]])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IsoDesc {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub status: i32,
    pub data: Vec<u8>,
    /// For OUT transfers; IN transfers report `data.len()`.
    pub actual_length: Option<u32>,
    pub iso: Vec<IsoDesc>,
    pub start_frame: u32,
}

impl Completion {
    pub fn ok(data: Vec<u8>) -> Self {
        Self {
            status: 0,
            data,
            actual_length: None,
            iso: Vec::new(),
            start_frame: 0,
        }
    }

    pub fn written(length: u32) -> Self {
        Self {
            actual_length: Some(length),
            ..Self::ok(Vec::new())
        }
    }

    pub fn status(status: i32) -> Self {
        Self {
            status,
            ..Self::ok(Vec::new())
        }
    }
}

pub enum Outcome {
    Complete(Completion),
    /// Writes these bytes verbatim instead of a well formed reply.
    Raw(Vec<u8>),
    /// Drops the connection.
    Disconnect,
}

pub type Handled = Pin<Box<dyn Future<Output = Outcome> + Send>>;

pub trait Device: Send + Sync + 'static {
    /// `(bEndpointAddress, bmAttributes)` of every non-control endpoint, used to reject URBs the
    /// way Linux does.
    fn endpoints(&self) -> Vec<(u8, u8)> {
        Vec::new()
    }

    /// Called once per CMD_SUBMIT. The returned future may wait indefinitely; it is dropped
    /// when the URB is unlinked.
    fn submit(&self, urb: Urb) -> Handled;

    /// A USB reset of the device, which `usbip-host` performs for a hub port-reset request.
    fn reset(&self) {}
}

/// SET_FEATURE(PORT_RESET) addressed to a hub port, which `usbip-host` turns into a device reset.
pub const PORT_RESET_SETUP: [u8; 4] = [0x23, 0x03, 0x04, 0x00];

impl<F, Fut> Device for F
where
    F: Fn(Urb) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Outcome> + Send + 'static,
{
    fn submit(&self, urb: Urb) -> Handled {
        Box::pin(self(urb))
    }
}

#[derive(Clone)]
pub struct Export {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub class: [u8; 3],
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub interfaces: Vec<[u8; 3]>,
    pub device: Arc<dyn Device>,
}

impl Export {
    pub fn new(busid: &str, device: impl Device) -> Self {
        let mut parts = busid.split(['-', '.']);
        let busnum = parts.next().and_then(|p| p.parse().ok()).unwrap_or(1);
        let devnum = parts.next().and_then(|p| p.parse().ok()).unwrap_or(2);
        Self {
            path: format!("/sys/devices/platform/vhci_hcd.0/usb{busnum}/{busid}"),
            busid: busid.into(),
            busnum,
            devnum,
            speed: 3,
            id_vendor: 0x1209,
            id_product: 0x0001,
            bcd_device: 0x0100,
            class: [0, 0, 0],
            configuration_value: 1,
            num_configurations: 1,
            interfaces: vec![[0xff, 0, 0]],
            device: Arc::new(device),
        }
    }

    pub fn devid(&self) -> u32 {
        (self.busnum << 16) | self.devnum
    }

    fn encode(&self, out: &mut Vec<u8>, with_interfaces: bool) {
        let mut path = [0u8; 256];
        path[..self.path.len()].copy_from_slice(self.path.as_bytes());
        let mut busid = [0u8; 32];
        busid[..self.busid.len()].copy_from_slice(self.busid.as_bytes());
        out.extend_from_slice(&path);
        out.extend_from_slice(&busid);
        out.extend_from_slice(&self.busnum.to_be_bytes());
        out.extend_from_slice(&self.devnum.to_be_bytes());
        out.extend_from_slice(&self.speed.to_be_bytes());
        out.extend_from_slice(&self.id_vendor.to_be_bytes());
        out.extend_from_slice(&self.id_product.to_be_bytes());
        out.extend_from_slice(&self.bcd_device.to_be_bytes());
        out.extend_from_slice(&self.class);
        out.push(self.configuration_value);
        out.push(self.num_configurations);
        out.push(self.interfaces.len() as u8);
        if with_interfaces {
            for i in &self.interfaces {
                out.extend_from_slice(i);
                out.push(0);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    DevList,
    Import(String),
    Submit(Urb),
    Unlink {
        seqnum: u32,
        target: u32,
        found: bool,
    },
    /// A RET_SUBMIT was sent for this seqnum.
    Completed(u32),
    Disconnected,
    Reset,
}

#[derive(Default)]
pub struct Log {
    pub events: Mutex<Vec<Event>>,
    /// Protocol violations by the client.
    pub violations: Mutex<Vec<String>>,
    pub connections: Mutex<u32>,
}

impl Log {
    fn push(&self, e: Event) {
        self.events.lock().unwrap().push(e);
    }

    fn violation(&self, v: String) {
        self.violations.lock().unwrap().push(v);
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    pub fn submits(&self) -> Vec<Urb> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Submit(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    /// The most URBs that were outstanding at once on endpoint number `ep`.
    pub fn max_in_flight(&self, ep: u32) -> usize {
        let mut on_ep = std::collections::HashSet::new();
        let (mut now, mut max) = (0usize, 0usize);
        for e in self.events() {
            match e {
                Event::Submit(u) if u.ep == ep => {
                    on_ep.insert(u.seqnum);
                    now += 1;
                    max = max.max(now);
                }
                Event::Completed(s) if on_ep.remove(&s) => now -= 1,
                _ => {}
            }
        }
        max
    }

    pub fn unlinks(&self) -> Vec<(u32, u32, bool)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Unlink {
                    seqnum,
                    target,
                    found,
                } => Some((seqnum, target, found)),
                _ => None,
            })
            .collect()
    }

    pub fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }
}

pub struct Server {
    pub addr: SocketAddr,
    pub log: Arc<Log>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start(exports: Vec<Export>) -> Self {
        Self::bind("127.0.0.1:0", exports).await
    }

    pub async fn bind(addr: &str, exports: Vec<Export>) -> Self {
        let listener = TcpListener::bind(addr).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Log::default());
        let exports = Arc::new(exports);
        let imported = Arc::new(Mutex::new(Vec::<String>::new()));
        let task_log = Arc::clone(&log);
        let task = tokio::spawn(async move {
            let mut connections = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                *task_log.connections.lock().unwrap() += 1;
                connections.push(AbortOnDrop(tokio::spawn(serve(
                    stream,
                    Arc::clone(&exports),
                    Arc::clone(&imported),
                    Arc::clone(&task_log),
                ))));
            }
        });
        Self { addr, log, task }
    }

    pub fn address(&self) -> String {
        self.addr.to_string()
    }

    pub async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr).await.unwrap()
    }

    pub fn assert_clean(&self) {
        assert_eq!(self.log.violations(), Vec::<String>::new());
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(
    mut stream: TcpStream,
    exports: Arc<Vec<Export>>,
    imported: Arc<Mutex<Vec<String>>>,
    log: Arc<Log>,
) {
    stream.set_nodelay(true).unwrap();
    let mut header = [0u8; 8];
    if stream.read_exact(&mut header).await.is_err() {
        return;
    }
    let version = u16::from_be_bytes([header[0], header[1]]);
    let code = u16::from_be_bytes([header[2], header[3]]);
    let status = u32::from_be_bytes(header[4..8].try_into().unwrap());
    if version != 0x0111 {
        log.violation(format!("op version {version:#06x}"));
    }
    if status != 0 {
        log.violation(format!("op request status {status}"));
    }
    match code {
        0x8005 => {
            log.push(Event::DevList);
            let mut out = vec![0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0];
            out.extend_from_slice(&(exports.len() as u32).to_be_bytes());
            for e in exports.iter() {
                e.encode(&mut out, true);
            }
            let _ = stream.write_all(&out).await;
        }
        0x8003 => {
            let mut busid = [0u8; 32];
            if stream.read_exact(&mut busid).await.is_err() {
                return;
            }
            let end = busid.iter().position(|&b| b == 0).unwrap_or(32);
            let busid = String::from_utf8_lossy(&busid[..end]).into_owned();
            log.push(Event::Import(busid.clone()));
            let export = exports.iter().find(|e| e.busid == busid).cloned();
            let available = export.is_some() && {
                let mut imported = imported.lock().unwrap();
                if imported.contains(&busid) {
                    false
                } else {
                    imported.push(busid.clone());
                    true
                }
            };
            let mut out = vec![0x01, 0x11, 0x00, 0x03];
            match export.filter(|_| available) {
                Some(export) => {
                    out.extend_from_slice(&0u32.to_be_bytes());
                    export.encode(&mut out, false);
                    if stream.write_all(&out).await.is_ok() {
                        serve_urbs(stream, export, &log).await;
                    }
                    imported.lock().unwrap().retain(|b| *b != busid);
                }
                None => {
                    out.extend_from_slice(&1u32.to_be_bytes());
                    let _ = stream.write_all(&out).await;
                }
            }
        }
        other => log.violation(format!("unknown op code {other:#06x}")),
    }
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
}

/// Cancels a submitted URB, whether it is queued behind others or already running.
type Cancel = tokio::sync::oneshot::Sender<()>;

struct Job {
    urb: Urb,
    future: Handled,
    cancel: tokio::sync::oneshot::Receiver<()>,
}

async fn serve_urbs(stream: TcpStream, export: Export, log: &Arc<Log>) {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    let writer_task = tokio::spawn(async move {
        while let Some(Some(frame)) = rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
    let pending: Arc<Mutex<HashMap<u32, Cancel>>> = Arc::default();
    // Like real hardware, each endpoint completes its URBs one at a time and in order.
    let mut queues: HashMap<u8, mpsc::UnboundedSender<Job>> = HashMap::new();
    let mut workers = Vec::new();

    loop {
        let mut h = [0u8; 48];
        if reader.read_exact(&mut h).await.is_err() {
            break;
        }
        let command = be32(&h, 0);
        let seqnum = be32(&h, 1);
        let devid = be32(&h, 2);
        if devid != export.devid() {
            log.violation(format!("seqnum {seqnum}: devid {devid:#x}"));
        }
        if pending.lock().unwrap().contains_key(&seqnum) {
            log.violation(format!("seqnum {seqnum} reused while pending"));
        }
        match command {
            1 => {
                let mut urb = Urb {
                    seqnum,
                    devid,
                    direction: be32(&h, 3),
                    ep: be32(&h, 4),
                    transfer_flags: be32(&h, 5),
                    length: be32(&h, 6),
                    start_frame: be32(&h, 7),
                    number_of_packets: be32(&h, 8),
                    interval: be32(&h, 9),
                    setup: h[40..48].try_into().unwrap(),
                    data: Vec::new(),
                    iso: Vec::new(),
                };
                if urb.direction > 1 || urb.ep > 15 {
                    log.violation(format!(
                        "seqnum {seqnum}: direction {} ep {}",
                        urb.direction, urb.ep
                    ));
                }
                if urb.ep != 0 && urb.setup != [0; 8] {
                    log.violation(format!("seqnum {seqnum}: setup bytes on ep {}", urb.ep));
                }
                if urb.length > 16 << 20 {
                    log.violation(format!("seqnum {seqnum}: length {}", urb.length));
                    break;
                }
                if !urb.is_in() {
                    urb.data = vec![0; urb.length as usize];
                    if reader.read_exact(&mut urb.data).await.is_err() {
                        break;
                    }
                }
                let packets = if urb.number_of_packets == 0xffff_ffff {
                    0
                } else {
                    urb.number_of_packets
                };
                if packets > 1024 {
                    log.violation(format!("seqnum {seqnum}: {packets} packets"));
                    break;
                }
                let mut iso = vec![0u8; packets as usize * 16];
                if reader.read_exact(&mut iso).await.is_err() {
                    break;
                }
                urb.iso = iso
                    .as_chunks::<16>()
                    .0
                    .iter()
                    .map(|c| IsoDesc {
                        offset: be32(c, 0),
                        length: be32(c, 1),
                        actual_length: be32(c, 2),
                        status: be32(c, 3) as i32,
                    })
                    .collect();
                log.push(Event::Submit(urb.clone()));
                let address = urb.ep as u8 | if urb.is_in() { 0x80 } else { 0 };
                let attributes = export
                    .device
                    .endpoints()
                    .iter()
                    .find(|e| e.0 == address)
                    .map(|e| e.1);
                if let Some(attributes) = attributes {
                    let periodic = matches!(attributes & 3, 1 | 3);
                    if periodic && urb.interval == 0 {
                        // Linux fails usb_submit_urb with -EINVAL and drops the connection.
                        log.violation(format!(
                            "seqnum {seqnum}: periodic endpoint {address:#04x} with interval 0"
                        ));
                        break;
                    }
                    if attributes & 3 == 1 && urb.iso.is_empty() {
                        log.violation(format!("seqnum {seqnum}: isochronous URB without packets"));
                        break;
                    }
                }

                let future: Handled = if urb.ep == 0 && urb.setup[..4] == PORT_RESET_SETUP {
                    log.push(Event::Reset);
                    export.device.reset();
                    Box::pin(async { Outcome::Complete(Completion::written(0)) })
                } else {
                    export.device.submit(urb.clone())
                };
                let (cancel_tx, cancel) = tokio::sync::oneshot::channel();
                pending.lock().unwrap().insert(seqnum, cancel_tx);
                let address = urb.ep as u8 | if urb.is_in() { 0x80 } else { 0 };
                let queue = queues.entry(address).or_insert_with(|| {
                    let (queue, jobs) = mpsc::unbounded_channel();
                    workers.push(AbortOnDrop(tokio::spawn(endpoint_worker(
                        jobs,
                        tx.clone(),
                        Arc::clone(&pending),
                        Arc::clone(log),
                    ))));
                    queue
                });
                let _ = queue.send(Job {
                    urb,
                    future,
                    cancel,
                });
            }
            2 => {
                let target = be32(&h, 5);
                if h[24..48].iter().any(|&b| b != 0) {
                    log.violation(format!("seqnum {seqnum}: unlink padding not zero"));
                }
                let mut pending = pending.lock().unwrap();
                let found = match pending.remove(&target) {
                    Some(cancel) => {
                        let _ = cancel.send(());
                        true
                    }
                    None => false,
                };
                log.push(Event::Unlink {
                    seqnum,
                    target,
                    found,
                });
                let status = if found { -ECONNRESET } else { 0 };
                let mut out = Vec::with_capacity(48);
                for v in [4u32, seqnum, 0, 0, 0, status as u32] {
                    out.extend_from_slice(&v.to_be_bytes());
                }
                out.resize(48, 0);
                let _ = tx.send(Some(out));
            }
            other => {
                log.violation(format!("unknown command {other}"));
                break;
            }
        }
    }
    pending.lock().unwrap().clear();
    drop(workers);
    log.push(Event::Disconnected);
    let _ = tx.send(None);
    let _ = writer_task.await;
}

async fn endpoint_worker(
    mut jobs: mpsc::UnboundedReceiver<Job>,
    tx: mpsc::UnboundedSender<Option<Vec<u8>>>,
    pending: Arc<Mutex<HashMap<u32, Cancel>>>,
    log: Arc<Log>,
) {
    while let Some(Job {
        urb,
        future,
        mut cancel,
    }) = jobs.recv().await
    {
        let outcome = tokio::select! {
            outcome = future => outcome,
            _ = &mut cancel => continue,
        };
        // The unlink path removes the entry first, so a cancelled URB never replies.
        let mut pending = pending.lock().unwrap();
        if pending.remove(&urb.seqnum).is_none() {
            continue;
        }
        log.push(Event::Completed(urb.seqnum));
        let _ = match outcome {
            Outcome::Complete(c) => tx.send(Some(ret_submit(&urb, c))),
            Outcome::Raw(bytes) => tx.send(Some(bytes)),
            Outcome::Disconnect => tx.send(None),
        };
    }
}

fn ret_submit(urb: &Urb, c: Completion) -> Vec<u8> {
    let actual = if urb.is_in() {
        c.data.len() as u32
    } else {
        c.actual_length.unwrap_or(urb.length)
    };
    let packets = if c.iso.is_empty() && !urb.iso.is_empty() {
        urb.iso.len()
    } else {
        c.iso.len()
    };
    let mut out = Vec::with_capacity(48 + c.data.len());
    for v in [
        3u32,
        urb.seqnum,
        0,
        0,
        0,
        c.status as u32,
        actual,
        c.start_frame,
        packets as u32,
        0,
    ] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.resize(48, 0);
    if urb.is_in() {
        out.extend_from_slice(&c.data);
    }
    let iso = if c.iso.is_empty() {
        urb.iso.clone()
    } else {
        c.iso
    };
    for d in iso {
        for v in [d.offset, d.length, d.actual_length, d.status as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
    out
}

pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}
