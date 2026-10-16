use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
};

use bytes::BytesMut;
use log::{debug, warn};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot, watch},
};

use crate::protocol::{
    self, CmdSubmit, CmdUnlink, Direction, ExportedDevice, IsoPacketDescriptor, ProtocolError,
    Reply, RetSubmit,
};

pub async fn list_devices<S>(stream: &mut S) -> protocol::Result<Vec<ExportedDevice>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream
        .write_all(&protocol::encode_devlist_request())
        .await?;
    protocol::read_devlist_reply(stream).await
}

pub async fn import<S>(mut stream: S, busid: &str) -> protocol::Result<Session>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
    S: Unpin,
{
    stream
        .write_all(&protocol::encode_import_request(busid)?)
        .await?;
    let device = protocol::read_import_reply(&mut stream).await?;
    if device.busid != busid {
        return Err(ProtocolError::Invalid(format!(
            "imported busid {:?}, requested {busid:?}",
            device.busid
        )));
    }
    Ok(Session::start(stream, device))
}

#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub ep: u8,
    pub direction: Direction,
    pub transfer_flags: u32,
    pub length: u32,
    pub setup: [u8; 8],
    pub data: Vec<u8>,
    pub start_frame: u32,
    pub interval: u32,
    pub iso_packets: Vec<IsoPacketDescriptor>,
}

impl SubmitRequest {
    pub fn new(ep: u8, direction: Direction, length: u32) -> Self {
        Self {
            ep,
            direction,
            transfer_flags: 0,
            length,
            setup: [0; 8],
            data: Vec::new(),
            start_frame: 0,
            interval: 0,
            iso_packets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum TransferError {
    #[error("transfer was unlinked (status {0})")]
    Unlinked(i32),
    #[error("connection closed: {0}")]
    Disconnected(String),
    #[error("invalid request: {0}")]
    Invalid(String),
}

pub type TransferResult = Result<RetSubmit, TransferError>;

struct PendingSubmit {
    direction: Direction,
    length: u32,
    reply: oneshot::Sender<TransferResult>,
}

#[derive(Default)]
struct State {
    next_seqnum: u32,
    submits: HashMap<u32, PendingSubmit>,
    /// Unlink seqnum to the seqnum of the transfer it cancels.
    unlinks: HashMap<u32, u32>,
    /// Transfers resolved by RET_UNLINK, kept so a late RET_SUBMIT from a non-Linux server can
    /// still be parsed and dropped.
    unlinked: VecDeque<(u32, Direction, u32)>,
    closed: Option<String>,
}

struct Shared {
    devid: u32,
    state: Mutex<State>,
    frames: mpsc::UnboundedSender<BytesMut>,
    closed_tx: watch::Sender<Option<String>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn close(&self, reason: String) {
        let pending = {
            let mut state = self.lock();
            if state.closed.is_some() {
                return;
            }
            state.closed = Some(reason.clone());
            state.unlinks.clear();
            std::mem::take(&mut state.submits)
        };
        for (_, p) in pending {
            let _ = p
                .reply
                .send(Err(TransferError::Disconnected(reason.clone())));
        }
        self.closed_tx.send_replace(Some(reason));
    }
}

/// An imported device. Cheap to clone; all clones share one connection.
///
/// Requests are written by a dedicated writer task in the order they were issued, and replies
/// are matched to requests by a reader task, so any number of transfers may be in flight.
#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
    device: Arc<ExportedDevice>,
}

impl Session {
    /// Must be called from within a tokio runtime.
    pub fn start<S>(stream: S, device: ExportedDevice) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (frames, frames_rx) = mpsc::unbounded_channel();
        let (closed_tx, _) = watch::channel(None);
        let shared = Arc::new(Shared {
            devid: device.devid(),
            state: Mutex::new(State::default()),
            frames,
            closed_tx,
        });
        tokio::spawn(write_loop(Arc::clone(&shared), writer, frames_rx));
        tokio::spawn(read_loop(
            Arc::clone(&shared),
            BufReader::with_capacity(64 * 1024, reader),
        ));
        Self {
            shared,
            device: Arc::new(device),
        }
    }

    pub fn device(&self) -> &ExportedDevice {
        &self.device
    }

    /// Queues a CMD_SUBMIT. Returns its seqnum and a future resolving to the reply. Safe to call
    /// from any thread; the reply future may be awaited on any runtime.
    pub fn submit(
        &self,
        req: SubmitRequest,
    ) -> Result<(u32, impl Future<Output = TransferResult> + Send + 'static), TransferError> {
        if req.direction == Direction::Out && req.data.len() != req.length as usize {
            return Err(TransferError::Invalid(format!(
                "OUT data is {} bytes but length is {}",
                req.data.len(),
                req.length
            )));
        }
        if req.direction == Direction::In && !req.data.is_empty() {
            return Err(TransferError::Invalid("IN transfer carries data".into()));
        }
        let (tx, rx) = oneshot::channel();
        let seqnum = {
            let mut state = self.shared.lock();
            if let Some(reason) = &state.closed {
                return Err(TransferError::Disconnected(reason.clone()));
            }
            let seqnum = next_seqnum(&mut state);
            let cmd = CmdSubmit {
                seqnum,
                devid: self.shared.devid,
                direction: req.direction as u32,
                ep: u32::from(req.ep & 0x0f),
                transfer_flags: req.transfer_flags,
                transfer_buffer_length: req.length,
                start_frame: req.start_frame,
                interval: req.interval,
                setup: req.setup,
                data: req.data,
                iso_packets: req.iso_packets,
            };
            let mut frame = BytesMut::new();
            cmd.encode(&mut frame);
            // Registered before the frame is queued so the reply can never outrun it.
            state.submits.insert(
                seqnum,
                PendingSubmit {
                    direction: req.direction,
                    length: req.length,
                    reply: tx,
                },
            );
            // The writer only drops its receiver after closing the session, which fails this
            // entry through `close`.
            let _ = self.shared.frames.send(frame);
            seqnum
        };
        let reply = async move {
            rx.await
                .unwrap_or_else(|_| Err(TransferError::Disconnected("session dropped".into())))
        };
        Ok((seqnum, reply))
    }

    /// Asks the server to cancel a submitted transfer. Its reply future then resolves with
    /// either the completed transfer or [`TransferError::Unlinked`]. Unknown seqnums are ignored.
    pub fn unlink(&self, target: u32) {
        let mut state = self.shared.lock();
        if state.closed.is_some()
            || !state.submits.contains_key(&target)
            || state.unlinks.values().any(|&t| t == target)
        {
            return;
        }
        let seqnum = next_seqnum(&mut state);
        state.unlinks.insert(seqnum, target);
        let mut frame = BytesMut::new();
        CmdUnlink {
            seqnum,
            devid: self.shared.devid,
            unlink_seqnum: target,
        }
        .encode(&mut frame);
        let _ = self.shared.frames.send(frame);
        debug!("unlinking seqnum {target} with seqnum {seqnum}");
    }

    pub fn unlink_all(&self) {
        let targets: Vec<u32> = self.shared.lock().submits.keys().copied().collect();
        for target in targets {
            self.unlink(target);
        }
    }

    pub fn pending(&self) -> usize {
        self.shared.lock().submits.len()
    }

    pub fn is_closed(&self) -> bool {
        self.shared.lock().closed.is_some()
    }

    /// Resolves with the reason once the connection is gone.
    pub async fn closed(&self) -> String {
        let mut rx = self.shared.closed_tx.subscribe();
        loop {
            if let Some(reason) = rx.borrow_and_update().clone() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return "session dropped".into();
            }
        }
    }

    pub fn close(&self) {
        self.shared.close("closed by client".into());
    }
}

fn next_seqnum(state: &mut State) -> u32 {
    loop {
        state.next_seqnum = state.next_seqnum.wrapping_add(1);
        let s = state.next_seqnum;
        // 0 is avoided because some servers treat it as "no request".
        if s != 0 && !state.submits.contains_key(&s) && !state.unlinks.contains_key(&s) {
            return s;
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    shared: Arc<Shared>,
    mut writer: W,
    mut frames: mpsc::UnboundedReceiver<BytesMut>,
) {
    let mut closed = shared.closed_tx.subscribe();
    let mut batch = BytesMut::new();
    loop {
        let frame = tokio::select! {
            frame = frames.recv() => frame,
            _ = closed.wait_for(|c| c.is_some()) => None,
        };
        let Some(frame) = frame else { break };
        batch.unsplit(frame);
        while batch.len() < 256 * 1024 {
            match frames.try_recv() {
                Ok(f) => batch.unsplit(f),
                Err(_) => break,
            }
        }
        if let Err(e) = writer.write_all(&batch).await {
            shared.close(format!("write failed: {e}"));
            break;
        }
        batch.clear();
    }
    let _ = writer.shutdown().await;
}

async fn read_loop<R: AsyncRead + Unpin>(shared: Arc<Shared>, mut reader: R) {
    let reason = loop {
        tokio::select! {
            result = read_one(&shared, &mut reader) => {
                if let Err(e) = result {
                    break match e {
                        ProtocolError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                            "server closed the connection".to_string()
                        }
                        e => e.to_string(),
                    };
                }
            }
            _ = wait_closed(&shared) => return,
        }
    };
    warn!("session closed: {reason}");
    shared.close(reason);
}

async fn wait_closed(shared: &Shared) {
    let _ = shared.closed_tx.subscribe().wait_for(|c| c.is_some()).await;
}

async fn read_one<R: AsyncRead + Unpin>(shared: &Shared, reader: &mut R) -> protocol::Result<()> {
    let header = protocol::read_reply_header(reader).await?;
    match header {
        Reply::Submit { seqnum, .. } => {
            let (direction, length) = {
                let state = shared.lock();
                let unlinked = state.unlinked.iter().find(|u| u.0 == seqnum);
                match (state.submits.get(&seqnum), unlinked) {
                    (Some(p), _) => (p.direction, p.length),
                    (None, Some(&(_, direction, length))) => (direction, length),
                    // Without the request we cannot tell whether a payload follows.
                    (None, None) => {
                        return Err(ProtocolError::Invalid(format!(
                            "RET_SUBMIT for unknown seqnum {seqnum}"
                        )))
                    }
                }
            };
            let ret =
                protocol::read_submit_payload(reader, &header, direction == Direction::In, length)
                    .await?;
            let pending = {
                let mut state = shared.lock();
                state.unlinks.retain(|_, t| *t != seqnum);
                state.unlinked.retain(|u| u.0 != seqnum);
                state.submits.remove(&seqnum)
            };
            if let Some(p) = pending {
                let _ = p.reply.send(Ok(ret));
            }
        }
        Reply::Unlink(ret) => {
            let pending = {
                let mut state = shared.lock();
                let target = state.unlinks.remove(&ret.seqnum);
                let pending = target.and_then(|t| state.submits.remove(&t).map(|p| (t, p)));
                pending.map(|(target, p)| {
                    if state.unlinked.len() == 256 {
                        state.unlinked.pop_front();
                    }
                    state.unlinked.push_back((target, p.direction, p.length));
                    p
                })
            };
            debug!("RET_UNLINK seqnum {} status {}", ret.seqnum, ret.status);
            // Linux sends no RET_SUBMIT for a transfer it reports through RET_UNLINK.
            if let Some(p) = pending {
                let _ = p.reply.send(Err(TransferError::Unlinked(ret.status)));
            }
        }
    }
    Ok(())
}
