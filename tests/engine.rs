mod support;

use std::{sync::Arc, time::Duration};

use support::{
    device::*,
    server::{Completion, Event, Export, Outcome, Server, Urb},
    setup, Completed, FakeRing, SerialExecutor,
};
use usbip_macos::{
    client::{self, Session},
    engine::{EndpointKey, Engine},
    hci::{EndpointInfo, EndpointKind, Status, Transfer},
    protocol::URB_ISO_ASAP,
};

const DEV: u8 = 1;
const EP0: EndpointKey = (DEV, 0);

struct Harness {
    rt: tokio::runtime::Runtime,
    server: Server,
    device: LoopbackDevice,
    session: Session,
    exec: Arc<SerialExecutor>,
    engine: Engine<FakeRing>,
}

impl Harness {
    fn new() -> Self {
        let device = LoopbackDevice::default();
        Self::with_export(device.export("1-1"), device)
    }

    fn with_export(export: Export, device: LoopbackDevice) -> Self {
        support::init_logging();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let server = rt.block_on(Server::start(vec![export]));
        let session =
            rt.block_on(async { client::import(server.connect().await, "1-1").await.unwrap() });
        let exec = SerialExecutor::new();
        let engine = Engine::new(session.clone(), rt.handle().clone(), exec.clone());
        Self {
            rt,
            server,
            device,
            session,
            exec,
            engine,
        }
    }

    fn ring(&self, key: EndpointKey) -> FakeRing {
        let ring = FakeRing::new(&self.exec);
        let kind = match key.1 {
            0 => EndpointKind::Control,
            EP_INT_IN => EndpointKind::Interrupt,
            EP_ISO_IN | EP_ISO_OUT => EndpointKind::Isochronous,
            _ => EndpointKind::Bulk,
        };
        let info = EndpointInfo {
            kind,
            max_packet_size: 512,
            b_interval: 4,
        };
        let (engine, r) = (self.engine.clone(), ring.clone());
        self.exec.run(move || engine.add_endpoint(key, info, r));
        ring
    }

    fn doorbell(&self, key: EndpointKey) {
        let engine = self.engine.clone();
        self.exec.run(move || engine.doorbell(key));
    }

    fn on_queue<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Engine<FakeRing>) -> T + Send + 'static,
    ) -> T {
        let engine = self.engine.clone();
        self.exec.run(move || f(&engine))
    }

    fn in_flight(&self, key: EndpointKey) -> Option<u32> {
        self.on_queue(move |e| e.in_flight(key))
    }

    fn wait<F: FnMut() -> bool>(&self, what: &str, f: F) {
        self.rt.block_on(support::eventually(what, f));
    }

    fn control(
        &self,
        ring: &FakeRing,
        setup_bytes: [u8; 8],
        data: Option<Vec<u8>>,
    ) -> Vec<Completed> {
        let before = ring.completed().len();
        ring.push_setup(setup_bytes);
        let stages = if let Some(d) = data {
            ring.push_normal(d);
            3
        } else {
            2
        };
        ring.push_status();
        self.doorbell(EP0);
        ring.wait_completed(before + stages)[before..].to_vec()
    }

    fn finish(&self, rings: &[&FakeRing]) {
        for r in rings {
            assert_eq!(r.wrong_thread(), 0, "ring touched off the controller queue");
        }
        self.server.assert_clean();
    }
}

#[test]
fn device_reset_reaches_the_server() {
    let h = Harness::new();
    let ep0 = h.ring(EP0);
    h.control(&ep0, setup(0x00, 9, 1, 0, 0), None);
    assert_eq!(h.device.configuration(), 1);
    h.on_queue(|e| e.reset_device()).expect("reset");
    assert!(h.server.log.events().contains(&Event::Reset));
    assert_eq!(h.device.configuration(), 0, "the device itself was reset");
    let done = h.control(&ep0, setup(0x80, 8, 0, 0, 1), Some(vec![0xff]));
    assert_eq!(done[1].buffer, [0], "control works after the reset");
    h.finish(&[&ep0]);
}

#[test]
fn control_in_with_data_stage() {
    let h = Harness::new();
    let ring = h.ring(EP0);
    let done = h.control(&ring, setup(0x80, 6, 0x0100, 0, 18), Some(vec![0; 18]));
    assert_eq!(
        done[0].transfer,
        Transfer::Setup(setup(0x80, 6, 0x0100, 0, 18))
    );
    assert_eq!((done[1].status, done[1].length), (Status::Success, 18));
    assert_eq!(&done[1].buffer[..4], &[18, 1, 0x00, 0x02]);
    assert_eq!(done[2].transfer, Transfer::Status);
    assert_eq!(done[2].status, Status::Success);
    assert_eq!(
        h.server.log.submits().len(),
        1,
        "one URB per control transfer"
    );
    h.finish(&[&ring]);
}

#[test]
fn control_short_read_reports_actual_length() {
    let h = Harness::new();
    let ring = h.ring(EP0);
    let done = h.control(&ring, setup(0x80, 6, 0x0200, 0, 255), Some(vec![0xaa; 255]));
    let config_len = done[1].buffer[2] as u32;
    assert_eq!(
        done[1].length, config_len,
        "short packet length, not the buffer size"
    );
    assert!(
        done[1].buffer[config_len as usize..]
            .iter()
            .all(|&b| b == 0xaa),
        "tail untouched"
    );
    h.finish(&[&ring]);
}

#[test]
fn control_out_with_data_and_without() {
    let h = Harness::new();
    let ring = h.ring(EP0);
    let done = h.control(
        &ring,
        setup(0x40, REQ_WRITE, 0, 0, 4),
        Some(b"ping".to_vec()),
    );
    assert_eq!((done[1].status, done[1].length), (Status::Success, 4));
    let urb = h.server.log.submits()[0].clone();
    assert_eq!((urb.direction, urb.data.as_slice()), (0, &b"ping"[..]));

    let done = h.control(&ring, setup(0x00, 9, 1, 0, 0), None);
    assert_eq!(done.len(), 2);
    assert_eq!(done[1].status, Status::Success);
    assert_eq!(h.device.configuration(), 1);

    let done = h.control(&ring, setup(0xc0, REQ_READ, 0, 0, 64), Some(vec![0; 64]));
    assert_eq!(&done[1].buffer[..4], b"ping");
    assert_eq!(done[1].length, 4);
    h.finish(&[&ring]);
}

#[test]
fn control_stall_in_data_and_status_stage() {
    let h = Harness::new();
    let ring = h.ring(EP0);
    ring.push_setup(setup(0xc0, REQ_STALL, 0, 0, 8));
    ring.push_normal(vec![0; 8]);
    h.doorbell(EP0);
    let done = ring.wait_completed(2);
    assert_eq!(done[1].status, Status::StallError);

    // The host resets the endpoint after a stall and drops the rest of that control transfer.
    ring.state.0.lock().unwrap().queue.clear();
    h.on_queue(|e| e.reset_endpoint(EP0));
    let done = h.control(&ring, setup(0x40, REQ_STALL, 0, 0, 0), None);
    assert_eq!(
        done[1].status,
        Status::StallError,
        "no-data request stalls in the status stage"
    );

    let done = h.control(&ring, setup(0x80, 0, 0, 0, 2), Some(vec![9; 2]));
    assert_eq!(done[1].status, Status::Success, "endpoint usable again");
    h.finish(&[&ring]);
}

#[test]
fn data_stage_is_clamped_to_w_length() {
    let h = Harness::new();
    let ring = h.ring(EP0);
    let done = h.control(&ring, setup(0xc0, REQ_PATTERN, 0, 0, 4), Some(vec![0; 64]));
    assert_eq!(done[1].length, 4);
    assert_eq!(h.server.log.submits()[0].length, 4);
    h.finish(&[&ring]);
}

#[test]
fn data_or_status_without_setup_is_not_sent() {
    let h = Harness::new();
    let ring = h.ring(EP0);
    ring.push_normal(vec![0; 8]);
    ring.push_status();
    h.doorbell(EP0);
    let done = ring.wait_completed(2);
    assert_eq!(done[0].status, Status::Error);
    assert_eq!(done[1].status, Status::Success);
    assert!(h.server.log.submits().is_empty());
    h.finish(&[&ring]);
}

#[test]
fn bulk_out_then_in() {
    let h = Harness::new();
    let out = h.ring((DEV, EP_BULK_OUT));
    let inp = h.ring((DEV, EP_BULK_IN));
    out.push_normal(pattern(1000, 1));
    h.doorbell((DEV, EP_BULK_OUT));
    assert_eq!(out.wait_completed(1)[0].length, 1000);
    inp.push_normal(vec![0; 4096]);
    h.doorbell((DEV, EP_BULK_IN));
    let done = inp.wait_completed(1);
    assert_eq!(done[0].length, 1000);
    assert_eq!(&done[0].buffer[..1000], &pattern(1000, 1)[..]);
    let urbs = h.server.log.submits();
    assert_eq!((urbs[0].ep, urbs[0].direction), (1, 0));
    assert_eq!(
        (urbs[1].ep, urbs[1].direction, urbs[1].length),
        (1, 1, 4096)
    );
    h.finish(&[&out, &inp]);
}

#[test]
fn queued_transfers_are_processed_in_order() {
    let h = Harness::new();
    let out = h.ring((DEV, EP_BULK_OUT));
    for i in 0..50u8 {
        out.push_normal(vec![i; 10]);
    }
    h.doorbell((DEV, EP_BULK_OUT));
    assert_eq!(out.wait_completed(50).len(), 50);
    let inp = h.ring((DEV, EP_BULK_IN));
    inp.push_normal(vec![0; 500]);
    h.doorbell((DEV, EP_BULK_IN));
    let data = &inp.wait_completed(1)[0].buffer;
    let expected: Vec<u8> = (0..50u8).flat_map(|i| vec![i; 10]).collect();
    assert_eq!(&data[..], &expected[..]);
    h.finish(&[&out, &inp]);
}

#[test]
fn doorbell_storm_does_not_duplicate_transfers() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    inp.push_normal(vec![0; 64]);
    for _ in 0..100 {
        let engine = h.engine.clone();
        h.exec
            .execute_boxed(move || engine.doorbell((DEV, EP_BULK_IN)));
    }
    h.wait("submit", || h.server.log.submits().len() == 1);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        h.server.log.submits().len(),
        1,
        "the head transfer was submitted more than once"
    );
    h.finish(&[&inp]);
}

#[test]
fn endpoints_run_independently() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    let int = h.ring((DEV, EP_INT_IN));
    let ep0 = h.ring(EP0);
    inp.push_normal(vec![0; 64]);
    h.doorbell((DEV, EP_BULK_IN));
    for _ in 0..5 {
        int.push_normal(vec![0; 8]);
    }
    h.doorbell((DEV, EP_INT_IN));
    let ticks = int.wait_completed(5);
    let counters: Vec<u64> = ticks
        .iter()
        .map(|c| u64::from_le_bytes(c.buffer[..8].try_into().unwrap()))
        .collect();
    assert_eq!(counters, vec![1, 2, 3, 4, 5]);
    h.control(&ep0, setup(0x80, 0, 0, 0, 2), Some(vec![0; 2]));
    assert!(
        inp.completed().is_empty(),
        "blocked bulk IN does not hold up others"
    );
    assert!(h.in_flight((DEV, EP_BULK_IN)).is_some());
    h.finish(&[&inp, &int, &ep0]);
}

#[test]
fn reset_cancels_in_flight_and_ignores_its_late_completion() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    inp.push_normal(vec![0xee; 16]);
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("submit", || h.server.log.submits().len() == 1);
    let seqnum = h.in_flight((DEV, EP_BULK_IN)).unwrap();

    h.on_queue(|e| e.reset_endpoint((DEV, EP_BULK_IN)));
    h.wait("unlink", || h.server.log.unlinks().len() == 1);
    assert_eq!(h.server.log.unlinks()[0].1, seqnum);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        inp.completed().is_empty(),
        "the cancelled transfer must not be completed"
    );
    assert_eq!(
        inp.state.0.lock().unwrap().queue[0].1,
        vec![0xee; 16],
        "buffer untouched"
    );
    assert_eq!(h.session.pending(), 0);

    // The host restarts the endpoint and rings again; the transfer is resubmitted.
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("resubmit", || h.server.log.submits().len() == 2);
    let out = h.ring((DEV, EP_BULK_OUT));
    out.push_normal(vec![7; 3]);
    h.doorbell((DEV, EP_BULK_OUT));
    let done = inp.wait_completed(1);
    assert_eq!(
        (done[0].length, &done[0].buffer[..3]),
        (3, &[7u8, 7, 7][..])
    );
    h.finish(&[&inp, &out]);
}

#[test]
fn completion_arriving_after_reset_is_dropped() {
    // The first reply is already queued on the executor when the endpoint is reset and the
    // transfer resubmitted; it must not complete the resubmitted transfer.
    let n = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let device = move |urb: Urb| {
        let tag = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        async move {
            tokio::time::sleep(Duration::from_millis(if tag == 1 { 30 } else { 200 })).await;
            Outcome::Complete(Completion::ok(vec![tag; urb.length as usize]))
        }
    };
    let h = Harness::with_export(Export::new("1-1", device), LoopbackDevice::default());
    let inp = h.ring((DEV, 0x81));
    inp.push_normal(vec![0; 4]);
    h.doorbell((DEV, 0x81));
    h.wait("submit", || h.server.log.submits().len() == 1);
    let engine = h.engine.clone();
    h.exec.run(move || {
        std::thread::sleep(Duration::from_millis(100));
        engine.reset_endpoint((DEV, 0x81));
        engine.doorbell((DEV, 0x81));
    });
    let done = inp.wait_completed(1);
    assert_eq!(done[0].buffer, vec![2; 4], "completed with the stale reply");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(inp.completed().len(), 1);
    h.finish(&[&inp]);
}

#[test]
fn removing_endpoint_or_device_cancels_transfers() {
    let h = Harness::new();
    let a = h.ring((DEV, EP_BULK_IN));
    let b = h.ring((2, EP_BULK_IN));
    a.push_normal(vec![0; 8]);
    b.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_BULK_IN));
    h.doorbell((2, EP_BULK_IN));
    h.wait("submits", || h.server.log.submits().len() == 2);
    h.on_queue(|e| e.remove_endpoint((DEV, EP_BULK_IN)));
    h.wait("first unlink", || h.server.log.unlinks().len() == 1);
    h.on_queue(|e| e.remove_device(2));
    h.wait("second unlink", || h.server.log.unlinks().len() == 2);
    assert!(!h.on_queue(|e| e.has_endpoint((DEV, EP_BULK_IN)) || e.has_endpoint((2, EP_BULK_IN))));
    h.doorbell((DEV, EP_BULK_IN));
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        h.server.log.submits().len(),
        2,
        "doorbell on a removed endpoint is ignored"
    );
    h.wait("drained", || h.session.pending() == 0);
    h.finish(&[&a, &b]);
}

#[test]
fn recreating_an_endpoint_replaces_the_old_one() {
    let h = Harness::new();
    let old = h.ring((DEV, EP_BULK_IN));
    old.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("submit", || h.server.log.submits().len() == 1);
    let new = h.ring((DEV, EP_BULK_IN));
    h.wait("unlink", || h.server.log.unlinks().len() == 1);
    assert!(h.in_flight((DEV, EP_BULK_IN)).is_none());
    assert_eq!(new.completed().len() + old.completed().len(), 0);
    h.finish(&[&old, &new]);
}

#[test]
fn inactive_ring_is_left_alone() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_INT_IN));
    inp.set_active(false);
    inp.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_INT_IN));
    std::thread::sleep(Duration::from_millis(20));
    assert!(h.server.log.submits().is_empty());
    inp.set_active(true);
    h.doorbell((DEV, EP_INT_IN));
    assert_eq!(inp.wait_completed(1)[0].length, 8);
    h.finish(&[&inp]);
}

#[test]
fn rejected_completion_stops_processing() {
    let h = Harness::new();
    let int = h.ring((DEV, EP_INT_IN));
    int.push_normal(vec![0; 8]);
    int.push_normal(vec![0; 8]);
    int.state.0.lock().unwrap().reject_completions = true;
    h.doorbell((DEV, EP_INT_IN));
    h.wait("submit", || h.server.log.submits().len() == 1);
    h.wait("idle", || h.in_flight((DEV, EP_INT_IN)).is_none());
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(h.server.log.submits().len(), 1, "no runaway resubmission");
    h.finish(&[&int]);
}

#[test]
fn link_messages() {
    let h = Harness::new();
    let int = h.ring((DEV, EP_INT_IN));
    int.push(
        Transfer::Link {
            needs_response: true,
        },
        vec![],
    );
    int.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_INT_IN));
    let done = int.wait_completed(2);
    assert_eq!(
        done[0].transfer,
        Transfer::Link {
            needs_response: true
        }
    );
    assert_eq!(done[1].length, 8);

    int.push(
        Transfer::Link {
            needs_response: false,
        },
        vec![],
    );
    int.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_INT_IN));
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        int.completed().len(),
        2,
        "a link that needs no response is left to the host"
    );
    h.finish(&[&int]);
}

#[test]
fn isochronous_in_and_out() {
    let h = Harness::new();
    let iso_in = h.ring((DEV, EP_ISO_IN));
    iso_in.push(
        Transfer::Isochronous {
            length: 64,
            frame: None,
        },
        vec![0; 64],
    );
    iso_in.push(
        Transfer::Isochronous {
            length: 32,
            frame: Some(17),
        },
        vec![0; 32],
    );
    h.doorbell((DEV, EP_ISO_IN));
    let done = iso_in.wait_completed(2);
    assert_eq!((done[0].status, done[0].length), (Status::Success, 64));
    assert_eq!(&done[0].buffer[..], &pattern(64, 0)[..]);
    let urbs = h.server.log.submits();
    assert_eq!(urbs[0].transfer_flags & URB_ISO_ASAP, URB_ISO_ASAP);
    assert_eq!(
        (urbs[1].transfer_flags & URB_ISO_ASAP, urbs[1].start_frame),
        (0, 17)
    );
    for u in &urbs {
        assert_eq!(u.number_of_packets, 1);
        assert_eq!(u.iso[0].length, u.length);
        assert!(u.interval > 0);
    }

    let iso_out = h.ring((DEV, EP_ISO_OUT));
    iso_out.push(
        Transfer::Isochronous {
            length: 16,
            frame: None,
        },
        vec![5; 16],
    );
    h.doorbell((DEV, EP_ISO_OUT));
    assert_eq!(iso_out.wait_completed(1)[0].length, 16);
    assert_eq!(h.device.iso_out(), vec![5; 16]);
    h.finish(&[&iso_in, &iso_out]);
}

#[test]
fn iso_packet_error_is_reported() {
    let failing = |urb: Urb| async move {
        let mut iso = urb.iso.clone();
        iso[0].status = -75;
        Outcome::Complete(Completion {
            iso,
            ..Completion::ok(Vec::new())
        })
    };
    let h = Harness::with_export(Export::new("1-1", failing), LoopbackDevice::default());
    let ring = h.ring((DEV, 0x83));
    ring.push(
        Transfer::Isochronous {
            length: 8,
            frame: None,
        },
        vec![0; 8],
    );
    h.doorbell((DEV, 0x83));
    assert_eq!(ring.wait_completed(1)[0].status, Status::OverrunError);
}

#[test]
fn halted_bulk_endpoint_reports_stall() {
    let h = Harness::new();
    let ep0 = h.ring(EP0);
    h.control(
        &ep0,
        setup(0x40, REQ_HALT, u16::from(EP_BULK_OUT), 0, 0),
        None,
    );
    let out = h.ring((DEV, EP_BULK_OUT));
    out.push_normal(vec![1; 4]);
    h.doorbell((DEV, EP_BULK_OUT));
    assert_eq!(out.wait_completed(1)[0].status, Status::StallError);
    h.control(&ep0, setup(0x02, 1, 0, u16::from(EP_BULK_OUT), 0), None);
    out.push_normal(vec![1; 4]);
    h.doorbell((DEV, EP_BULK_OUT));
    assert_eq!(out.wait_completed(2)[1].status, Status::Success);
    h.finish(&[&ep0, &out]);
}

#[test]
fn disconnect_completes_everything_offline_without_spinning() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    let ep0 = h.ring(EP0);
    inp.push_normal(vec![0; 8]);
    inp.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("both submitted", || h.server.log.submits().len() == 2);
    ep0.push_setup(setup(0x40, REQ_DISCONNECT, 0, 0, 0));
    ep0.push_status();
    h.doorbell(EP0);
    let done = inp.wait_completed(2);
    assert!(done.iter().all(|c| c.status == Status::Offline), "{done:?}");
    assert_eq!(ep0.wait_completed(2)[1].status, Status::Offline);
    inp.push_normal(vec![0; 8]);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        inp.queued(),
        1,
        "nothing new is processed once the connection is gone"
    );
    h.doorbell((DEV, EP_BULK_IN));
    assert_eq!(inp.wait_completed(3)[2].status, Status::Offline);
    assert_eq!(
        h.server.log.submits().len(),
        3,
        "no submissions after the disconnect"
    );
}

#[test]
fn stress_many_endpoints_with_resets() {
    let h = Harness::new();
    let int = h.ring((DEV, EP_INT_IN));
    let out = h.ring((DEV, EP_BULK_OUT));
    let inp = h.ring((DEV, EP_BULK_IN));
    let ep0 = h.ring(EP0);
    for round in 0..30u32 {
        for _ in 0..4 {
            int.push_normal(vec![0; 8]);
        }
        out.push_normal(vec![round as u8; 100]);
        inp.push_normal(vec![0; 100]);
        ep0.push_setup(setup(0xc0, REQ_PATTERN, round as u16, 0, 32));
        ep0.push_normal(vec![0; 32]);
        ep0.push_status();
        for key in [(DEV, EP_INT_IN), (DEV, EP_BULK_OUT), (DEV, EP_BULK_IN), EP0] {
            let engine = h.engine.clone();
            h.exec.execute_boxed(move || engine.doorbell(key));
        }
        if round % 5 == 4 {
            let engine = h.engine.clone();
            h.exec
                .execute_boxed(move || engine.reset_endpoint((DEV, EP_INT_IN)));
            let engine = h.engine.clone();
            h.exec
                .execute_boxed(move || engine.doorbell((DEV, EP_INT_IN)));
        }
        let n = (round as usize + 1) * 3;
        let done = ep0.wait_completed(n);
        assert_eq!(&done[n - 2].buffer[..], &pattern(32, round as u8)[..]);
        let got = inp.wait_completed(round as usize + 1);
        assert_eq!(got[round as usize].buffer, vec![round as u8; 100]);
    }
    int.wait_completed(120);
    h.wait("drained", || h.session.pending() == 0);
    h.finish(&[&int, &out, &inp, &ep0]);
}

#[test]
fn pause_between_setup_and_data_keeps_the_setup() {
    let h = Harness::new();
    let ring = h.ring(EP0);
    ring.push_setup(setup(0xc0, REQ_PATTERN, 4, 0, 16));
    h.doorbell(EP0);
    ring.wait_completed(1);
    ring.set_active(false);
    h.on_queue(|e| e.sync_endpoint(EP0));
    ring.push_normal(vec![0; 16]);
    ring.push_status();
    ring.set_active(true);
    h.doorbell(EP0);
    let done = ring.wait_completed(3);
    assert_eq!(
        (done[1].status, &done[1].buffer[..]),
        (Status::Success, &pattern(16, 4)[..])
    );
    assert_eq!(done[2].status, Status::Success);
    h.finish(&[&ring]);
}

fn bulk_in_submits(h: &Harness) -> usize {
    h.server
        .log
        .submits()
        .iter()
        .filter(|u| u.ep == 1 && u.is_in())
        .count()
}

#[test]
fn bulk_transfers_are_pipelined() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    for _ in 0..20 {
        inp.push_normal(vec![0; 16]);
    }
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("pipeline filled", || bulk_in_submits(&h) == 8);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(bulk_in_submits(&h), 8, "depth is bounded");
    assert_eq!(h.on_queue(|e| e.in_flight_count((DEV, EP_BULK_IN))), 8);
    let out = h.ring((DEV, EP_BULK_OUT));
    for i in 0..20u8 {
        out.push_normal(vec![i; 16]);
    }
    h.doorbell((DEV, EP_BULK_OUT));
    let done = inp.wait_completed(20);
    for (i, c) in done.iter().enumerate() {
        assert_eq!(c.buffer, vec![i as u8; 16], "completion {i} out of order");
    }
    h.finish(&[&inp, &out]);
}

#[test]
fn moving_the_ring_aborts_every_in_flight_transfer() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    inp.push_normal(vec![0xee; 64]);
    inp.push_normal(vec![0; 64]);
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("both submitted", || bulk_in_submits(&h) == 2);

    inp.set_active(false);
    h.on_queue(|e| e.sync_endpoint((DEV, EP_BULK_IN)));
    assert!(
        h.server.log.unlinks().is_empty(),
        "sync without movement keeps the transfers"
    );
    inp.skip_head();
    h.on_queue(|e| e.sync_endpoint((DEV, EP_BULK_IN)));
    h.wait("both unlinked", || h.server.log.unlinks().len() == 2);
    assert!(
        h.server.log.unlinks().iter().all(|u| u.2),
        "both were still pending"
    );

    inp.set_active(true);
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("resubmitted for the new head", || bulk_in_submits(&h) == 3);
    let out = h.ring((DEV, EP_BULK_OUT));
    out.push_normal(vec![3; 5]);
    h.doorbell((DEV, EP_BULK_OUT));
    let done = inp.wait_completed(1);
    assert_eq!(done[0].buffer[..5], [3; 5], "data went to the new head");
    h.finish(&[&inp, &out]);
}

#[test]
fn reply_for_a_skipped_message_is_dropped() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    inp.push_normal(vec![0; 8]);
    inp.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("both submitted", || bulk_in_submits(&h) == 2);
    // The host moves the ring without telling us before the first reply comes back.
    inp.skip_head();
    let out = h.ring((DEV, EP_BULK_OUT));
    out.push_normal(vec![1; 8]);
    h.doorbell((DEV, EP_BULK_OUT));
    h.wait("resubmitted for the new head", || bulk_in_submits(&h) == 3);
    out.push_normal(vec![2; 8]);
    h.doorbell((DEV, EP_BULK_OUT));
    let done = inp.wait_completed(1);
    assert_eq!(
        done[0].buffer,
        vec![2; 8],
        "a stale reply landed in the new head"
    );
    h.finish(&[&inp, &out]);
}

#[test]
fn pause_unlinks_a_read_and_resume_resubmits_it() {
    let h = Harness::new();
    let inp = h.ring((DEV, EP_BULK_IN));
    inp.push_normal(vec![0; 64]);
    h.doorbell((DEV, EP_BULK_IN));
    h.wait("submit", || h.server.log.submits().len() == 1);

    inp.set_active(false);
    h.on_queue(|e| e.pause_endpoint((DEV, EP_BULK_IN)));
    h.wait("unlink", || h.server.log.unlinks().len() == 1);
    assert!(
        h.server.log.unlinks()[0].2,
        "the pending read was cancelled on the server"
    );
    h.wait("unlink confirmed", || {
        h.in_flight((DEV, EP_BULK_IN)).is_none()
    });

    let out = h.ring((DEV, EP_BULK_OUT));
    out.push_normal(b"after pause".to_vec());
    h.doorbell((DEV, EP_BULK_OUT));
    out.wait_completed(1);
    assert!(inp.completed().is_empty(), "nothing completed while paused");

    inp.set_active(true);
    h.doorbell((DEV, EP_BULK_IN));
    let done = inp.wait_completed(1);
    assert_eq!(
        &done[0].buffer[..11],
        b"after pause",
        "no data lost to the paused read"
    );
    assert_eq!(bulk_in_submits(&h), 2);
    h.finish(&[&inp, &out]);
}

#[test]
fn control_request_survives_a_pause_without_repeating() {
    let h = Harness::new();
    let ep0 = h.ring(EP0);
    ep0.push_setup(setup(0xc0, REQ_PATTERN, 5, 0, 16));
    ep0.push_normal(vec![0; 16]);
    ep0.push_status();
    let (engine, ring) = (h.engine.clone(), ep0.clone());
    h.exec.run(move || {
        engine.doorbell(EP0);
        ring.set_active(false);
        engine.pause_endpoint(EP0);
    });
    h.wait("data stage submitted", || h.server.log.submits().len() == 1);
    std::thread::sleep(Duration::from_millis(50));
    ep0.set_active(true);
    h.doorbell(EP0);
    let done = ep0.wait_completed(3);
    assert_eq!(&done[1].buffer[..], &pattern(16, 5)[..]);
    assert!(h.server.log.unlinks().is_empty());
    assert_eq!(h.server.log.submits().len(), 1, "sent to the device once");
    h.finish(&[&ep0]);
}

#[test]
fn reply_during_a_pause_is_parked_and_delivered() {
    let h = Harness::new();
    let ep0 = h.ring(EP0);
    ep0.push_setup(setup(0xc0, REQ_PATTERN, 9, 0, 32));
    ep0.push_normal(vec![0; 32]);
    ep0.push_status();
    // Hold the queue so the reply is already in hand when the pause arrives.
    let (engine, ring) = (h.engine.clone(), ep0.clone());
    h.exec.run(move || {
        engine.doorbell(EP0);
        std::thread::sleep(Duration::from_millis(100));
        ring.set_active(false);
        engine.pause_endpoint(EP0);
    });
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        ep0.completed().len(),
        1,
        "only the setup completed while paused"
    );
    ep0.set_active(true);
    h.doorbell(EP0);
    let done = ep0.wait_completed(3);
    assert_eq!(&done[1].buffer[..], &pattern(32, 9)[..]);
    assert_eq!(
        h.server.log.submits().len(),
        1,
        "the completed URB was not resubmitted"
    );
    h.finish(&[&ep0]);
}

#[test]
fn periodic_transfers_carry_the_endpoint_interval() {
    let h = Harness::new();
    let int = h.ring((DEV, EP_INT_IN));
    let bulk = h.ring((DEV, EP_BULK_OUT));
    int.push_normal(vec![0; 8]);
    bulk.push_normal(vec![0; 8]);
    h.doorbell((DEV, EP_INT_IN));
    h.doorbell((DEV, EP_BULK_OUT));
    int.wait_completed(1);
    bulk.wait_completed(1);
    let urbs = h.server.log.submits();
    let interval = |ep: u32| urbs.iter().find(|u| u.ep == ep).unwrap().interval;
    assert_eq!(interval(2), 8, "bInterval 4 at high speed is 8 microframes");
    assert_eq!(interval(1), 0);
    h.finish(&[&int, &bulk]);
}

#[test]
fn isochronous_transfers_are_pipelined() {
    let h = Harness::new();
    let iso = h.ring((DEV, EP_ISO_OUT));
    for i in 0..100u8 {
        iso.push(
            Transfer::Isochronous {
                length: 16,
                frame: None,
            },
            vec![i; 16],
        );
    }
    h.doorbell((DEV, EP_ISO_OUT));
    let done = iso.wait_completed(100);
    assert!(done
        .iter()
        .all(|c| c.status == Status::Success && c.length == 16));
    let expected: Vec<u8> = (0..100u8).flat_map(|i| vec![i; 16]).collect();
    assert_eq!(
        h.device.iso_out(),
        expected,
        "packets reached the device in order"
    );
    let max_in_flight = h.server.log.max_in_flight(3);
    assert!(
        max_in_flight > 1 && max_in_flight <= 32,
        "in flight at once: {max_in_flight}"
    );
    h.finish(&[&iso]);
}

#[test]
fn isochronous_length_comes_from_the_packet() {
    // Linux reports an isochronous OUT URB's actual_length as 0 and the bytes per packet.
    let device = |urb: Urb| async move {
        let iso = urb
            .iso
            .iter()
            .map(|d| support::server::IsoDesc {
                actual_length: d.length,
                ..*d
            })
            .collect();
        Outcome::Complete(Completion {
            iso,
            ..Completion::written(0)
        })
    };
    let h = Harness::with_export(Export::new("1-1", device), LoopbackDevice::default());
    let iso = h.ring((DEV, EP_ISO_OUT));
    iso.push(
        Transfer::Isochronous {
            length: 24,
            frame: None,
        },
        vec![7; 24],
    );
    h.doorbell((DEV, EP_ISO_OUT));
    assert_eq!(iso.wait_completed(1)[0].length, 24);
}
