#![allow(dead_code)]

pub mod classes;
pub mod device;
#[cfg(all(target_os = "macos", test))]
pub mod macos;
pub mod server;
pub mod usb;

use std::{
    collections::VecDeque,
    sync::{mpsc, Arc, Condvar, Mutex},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

use usbip_macos::{
    engine::{Executor, Ring},
    hci::{Status, Transfer},
};

/// A serial executor on a dedicated thread, standing in for the controller's dispatch queue.
pub struct SerialExecutor {
    tx: Mutex<mpsc::Sender<Box<dyn FnOnce() + Send>>>,
    pub thread: ThreadId,
}

impl SerialExecutor {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let handle = thread::Builder::new()
            .name("controller-queue".into())
            .spawn(move || {
                while let Ok(work) = rx.recv() {
                    work();
                }
            })
            .unwrap();
        Arc::new(Self {
            tx: Mutex::new(tx),
            thread: handle.thread().id(),
        })
    }

    pub fn execute_boxed(&self, f: impl FnOnce() + Send + 'static) {
        self.execute(Box::new(f));
    }

    /// Runs `f` on the executor and waits for its result.
    pub fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        self.execute(Box::new(move || {
            let _ = tx.send(f());
        }));
        rx.recv().expect("executor stopped")
    }
}

impl Executor for SerialExecutor {
    fn execute(&self, work: Box<dyn FnOnce() + Send>) {
        self.tx
            .lock()
            .unwrap()
            .send(work)
            .expect("executor stopped");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    pub transfer: Transfer,
    pub status: Status,
    pub length: u32,
    /// The message buffer after completion.
    pub buffer: Vec<u8>,
}

#[derive(Default)]
pub struct RingState {
    pub queue: VecDeque<(Transfer, Vec<u8>)>,
    /// Identity of the message at the front of `queue`.
    pub head_id: u64,
    pub completed: Vec<Completed>,
    pub active: bool,
    pub reject_completions: bool,
    pub wrong_thread: u32,
}

/// An endpoint ring whose contents the test controls. Every access asserts that it happens on
/// the executor thread, which is the contract the macOS state machines impose.
#[derive(Clone)]
pub struct FakeRing {
    pub state: Arc<(Mutex<RingState>, Condvar)>,
    executor: ThreadId,
}

impl FakeRing {
    pub fn new(executor: &SerialExecutor) -> Self {
        Self {
            state: Arc::new((
                Mutex::new(RingState {
                    active: true,
                    ..Default::default()
                }),
                Condvar::new(),
            )),
            executor: executor.thread,
        }
    }

    pub fn push(&self, transfer: Transfer, buffer: Vec<u8>) {
        self.state
            .0
            .lock()
            .unwrap()
            .queue
            .push_back((transfer, buffer));
    }

    pub fn push_setup(&self, setup: [u8; 8]) {
        self.push(Transfer::Setup(setup), Vec::new());
    }

    pub fn push_normal(&self, buffer: Vec<u8>) {
        let length = buffer.len() as u32;
        self.push(Transfer::Normal { length }, buffer);
    }

    pub fn push_status(&self) {
        self.push(Transfer::Status, Vec::new());
    }

    pub fn completed(&self) -> Vec<Completed> {
        self.state.0.lock().unwrap().completed.clone()
    }

    pub fn queued(&self) -> usize {
        self.state.0.lock().unwrap().queue.len()
    }

    /// Drops the head message without completing it, as when the host moves the ring.
    pub fn skip_head(&self) {
        let mut s = self.state.0.lock().unwrap();
        s.queue.pop_front();
        s.head_id += 1;
    }

    pub fn set_active(&self, active: bool) {
        self.state.0.lock().unwrap().active = active;
    }

    pub fn wait_completed(&self, n: usize) -> Vec<Completed> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (lock, cv) = &*self.state;
        let mut s = lock.lock().unwrap();
        while s.completed.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for {n} completions, got {:?}",
                s.completed
            );
            s = cv.wait_timeout(s, left).unwrap().0;
        }
        s.completed.clone()
    }

    fn check_thread(&self, s: &mut RingState) {
        if thread::current().id() != self.executor {
            s.wrong_thread += 1;
        }
    }

    pub fn wrong_thread(&self) -> u32 {
        self.state.0.lock().unwrap().wrong_thread
    }
}

impl Ring for FakeRing {
    fn active(&mut self) -> bool {
        let mut s = self.state.0.lock().unwrap();
        self.check_thread(&mut s);
        s.active
    }

    fn current(&mut self) -> (u64, Option<Transfer>) {
        let mut s = self.state.0.lock().unwrap();
        self.check_thread(&mut s);
        (s.head_id, s.queue.front().map(|m| m.0))
    }

    fn peek(&mut self, ahead: usize) -> Option<(u64, Transfer)> {
        let mut s = self.state.0.lock().unwrap();
        self.check_thread(&mut s);
        match s.queue.get(ahead) {
            Some((Transfer::Link { .. }, _)) | None => None,
            Some((t, _)) => Some((s.head_id + ahead as u64, *t)),
        }
    }

    fn read(&mut self, ahead: usize, length: usize) -> Vec<u8> {
        let mut s = self.state.0.lock().unwrap();
        self.check_thread(&mut s);
        let buf = &s.queue.get(ahead).expect("read without message").1;
        buf[..length.min(buf.len())].to_vec()
    }

    fn complete(&mut self, status: Status, data: &[u8], length: u32) -> Result<(), String> {
        let (lock, cv) = &*self.state;
        let mut s = lock.lock().unwrap();
        self.check_thread(&mut s);
        if s.reject_completions || !s.active {
            return Err("endpoint not active".into());
        }
        let (transfer, mut buffer) = s.queue.pop_front().expect("complete without message");
        s.head_id += 1;
        assert!(data.len() <= buffer.len(), "data overruns buffer");
        buffer[..data.len()].copy_from_slice(data);
        s.completed.push(Completed {
            transfer,
            status,
            length,
            buffer,
        });
        cv.notify_all();
        Ok(())
    }
}

pub fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> [u8; 8] {
    let mut s = [request_type, request, 0, 0, 0, 0, 0, 0];
    s[2..4].copy_from_slice(&value.to_le_bytes());
    s[4..6].copy_from_slice(&index.to_le_bytes());
    s[6..8].copy_from_slice(&length.to_le_bytes());
    s
}

pub fn init_logging() {
    let _ = simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Warn)
        .env()
        .init();
}

/// Polls `f` until it returns true or the timeout elapses.
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
