mod support;

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use support::{
    device::*,
    eventually,
    server::{Completion, Event, Export, Outcome, Server, Urb},
};
use tokio::io::AsyncWriteExt;
use usbip_macos::{
    client::{self, Session, SubmitRequest, TransferError},
    protocol::{Direction, ProtocolError},
};

async fn loopback() -> (Server, LoopbackDevice, Session) {
    let dev = LoopbackDevice::default();
    let server = Server::start(vec![dev.export("1-1")]).await;
    let session = client::import(server.connect().await, "1-1").await.unwrap();
    (server, dev, session)
}

fn control_in(request: u8, value: u16, length: u16) -> SubmitRequest {
    let mut req = SubmitRequest::new(0, Direction::In, u32::from(length));
    req.setup = support::setup(0xc0, request, value, 0, length);
    req
}

fn bulk_out(data: Vec<u8>) -> SubmitRequest {
    let mut req = SubmitRequest::new(EP_BULK_OUT, Direction::Out, data.len() as u32);
    req.data = data;
    req
}

async fn run(session: &Session, req: SubmitRequest) -> client::TransferResult {
    let (_, reply) = session.submit(req)?;
    reply.await
}

#[tokio::test]
async fn lists_exported_devices() {
    let a = LoopbackDevice::default().export("1-1");
    let mut b = LoopbackDevice::default().export("2-3");
    b.id_product = 0x4242;
    let server = Server::start(vec![a, b]).await;
    let devices = client::list_devices(&mut server.connect().await)
        .await
        .unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].busid, "1-1");
    assert_eq!(devices[0].interfaces.len(), 2);
    assert_eq!(
        (devices[1].busnum, devices[1].devnum, devices[1].id_product),
        (2, 3, 0x4242)
    );
    assert_eq!(server.log.events(), vec![Event::DevList]);
    server.assert_clean();
}

#[tokio::test]
async fn empty_device_list() {
    let server = Server::start(vec![]).await;
    let devices = client::list_devices(&mut server.connect().await)
        .await
        .unwrap();
    assert!(devices.is_empty());
}

#[tokio::test]
async fn import_unknown_or_busy_device_fails() {
    let server = Server::start(vec![LoopbackDevice::default().export("1-1")]).await;
    let err = client::import(server.connect().await, "9-9")
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ProtocolError::Status(1)), "{err}");

    let _first = client::import(server.connect().await, "1-1").await.unwrap();
    let err = client::import(server.connect().await, "1-1")
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ProtocolError::Status(1)), "{err}");
}

#[tokio::test]
async fn import_rejects_mismatched_busid() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut req = [0u8; 40];
        tokio::io::AsyncReadExt::read_exact(&mut s, &mut req)
            .await
            .unwrap();
        let mut out = vec![0x01, 0x11, 0x00, 0x03, 0, 0, 0, 0];
        let mut dev = vec![0u8; 312];
        dev[256..259].copy_from_slice(b"7-7");
        out.extend(dev);
        s.write_all(&out).await.unwrap();
    });
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    assert!(matches!(
        client::import(stream, "1-1").await,
        Err(ProtocolError::Invalid(_))
    ));
}

#[tokio::test]
async fn devid_and_headers_are_correct() {
    let (server, _dev, session) = loopback().await;
    assert_eq!(session.device().devid(), 0x0001_0001);
    run(&session, control_in(REQ_PATTERN, 1, 16)).await.unwrap();
    let urb = &server.log.submits()[0];
    assert_eq!(urb.devid, 0x0001_0001);
    assert_eq!((urb.direction, urb.ep, urb.length), (1, 0, 16));
    assert_eq!(urb.setup, support::setup(0xc0, REQ_PATTERN, 1, 0, 16));
    assert_eq!(urb.number_of_packets, 0);
    server.assert_clean();
}

#[tokio::test]
async fn control_transfers() {
    let (server, _dev, session) = loopback().await;
    let ret = run(&session, control_in(REQ_PATTERN, 3, 40)).await.unwrap();
    assert_eq!(ret.data, pattern(40, 3));
    assert_eq!(ret.status, 0);

    let mut write = SubmitRequest::new(0, Direction::Out, 5);
    write.setup = support::setup(0x40, REQ_WRITE, 0, 0, 5);
    write.data = b"hello".to_vec();
    let ret = run(&session, write).await.unwrap();
    assert_eq!(ret.actual_length, 5);
    let ret = run(&session, control_in(REQ_READ, 0, 64)).await.unwrap();
    assert_eq!(ret.data, b"hello");

    let short = run(&session, control_in(REQ_SHORT, 7, 64)).await.unwrap();
    assert_eq!(short.actual_length, 7);
    assert_eq!(short.data.len(), 7);

    let stalled = run(&session, control_in(REQ_STALL, 0, 8)).await.unwrap();
    assert_eq!(stalled.status, -32);
    server.assert_clean();
}

#[tokio::test]
async fn bulk_loopback_large_transfers() {
    let (server, _dev, session) = loopback().await;
    let payload = pattern(1 << 20, 9);
    let ret = run(&session, bulk_out(payload.clone())).await.unwrap();
    assert_eq!(ret.actual_length, 1 << 20);
    let mut received = Vec::new();
    while received.len() < payload.len() {
        let ret = run(
            &session,
            SubmitRequest::new(EP_BULK_IN, Direction::In, 64 * 1024),
        )
        .await
        .unwrap();
        received.extend(ret.data);
    }
    assert_eq!(received, payload);
    server.assert_clean();
}

#[tokio::test]
async fn invalid_requests_are_rejected_locally() {
    let (server, _dev, session) = loopback().await;
    let mut bad = SubmitRequest::new(EP_BULK_OUT, Direction::Out, 10);
    bad.data = vec![0; 3];
    assert!(matches!(
        session.submit(bad),
        Err(TransferError::Invalid(_))
    ));
    let mut bad = SubmitRequest::new(EP_BULK_IN, Direction::In, 10);
    bad.data = vec![0; 10];
    assert!(matches!(
        session.submit(bad),
        Err(TransferError::Invalid(_))
    ));
    assert!(server.log.submits().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_out_of_order_reach_the_right_caller() {
    let device = |urb: Urb| async move {
        let delay = 50 - (urb.setup[2] as u64 % 50);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        Outcome::Complete(Completion::ok(vec![urb.setup[2]; urb.length as usize]))
    };
    let server = Server::start(vec![Export::new("1-1", device)]).await;
    let session = client::import(server.connect().await, "1-1").await.unwrap();
    let mut tasks = Vec::new();
    for i in 0..50u8 {
        let mut req = SubmitRequest::new(0, Direction::In, 4);
        req.setup[2] = i;
        let (_, reply) = session.submit(req).unwrap();
        tasks.push(tokio::spawn(async move { (i, reply.await.unwrap()) }));
    }
    for t in tasks {
        let (i, ret) = t.await.unwrap();
        assert_eq!(ret.data, vec![i; 4]);
    }
    server.assert_clean();
}

#[test]
fn concurrent_submitters_from_many_threads() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let echo = |urb: Urb| async move {
        if urb.is_in() {
            Outcome::Complete(Completion::ok(urb.setup[..4].to_vec()))
        } else {
            Outcome::Complete(Completion::written(urb.length))
        }
    };
    let server = rt.block_on(Server::start(vec![Export::new("1-1", echo)]));
    let session =
        rt.block_on(async { client::import(server.connect().await, "1-1").await.unwrap() });
    let seqnums = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let threads: Vec<_> = (0..8u32)
        .map(|t| {
            let session = session.clone();
            let handle = rt.handle().clone();
            let seqnums = Arc::clone(&seqnums);
            std::thread::spawn(move || {
                for i in 0..250u32 {
                    let tag = (t << 16 | i).to_le_bytes();
                    let mut req = SubmitRequest::new(0, Direction::In, 4);
                    req.setup[..4].copy_from_slice(&tag);
                    let (seqnum, reply) = session.submit(req).unwrap();
                    assert!(
                        seqnums.lock().unwrap().insert(seqnum),
                        "duplicate seqnum {seqnum}"
                    );
                    let ret = handle.block_on(reply).unwrap();
                    assert_eq!(ret.data, tag);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(session.pending(), 0);
    assert_eq!(server.log.submits().len(), 2000);
    server.assert_clean();
}

#[tokio::test]
async fn reply_faster_than_registration_is_not_lost() {
    // A server that answers instantly exercises the window between writing a request and
    // registering it, which the previous client lost replies in.
    let instant =
        |urb: Urb| async move { Outcome::Complete(Completion::ok(vec![1; urb.length as usize])) };
    let server = Server::start(vec![Export::new("1-1", instant)]).await;
    let session = client::import(server.connect().await, "1-1").await.unwrap();
    for _ in 0..500 {
        let ret = tokio::time::timeout(
            Duration::from_secs(5),
            run(&session, SubmitRequest::new(0x81, Direction::In, 2)),
        )
        .await
        .expect("reply lost")
        .unwrap();
        assert_eq!(ret.data, vec![1, 1]);
    }
}

#[tokio::test]
async fn unlink_pending_transfer() {
    let (server, _dev, session) = loopback().await;
    let (seqnum, reply) = session
        .submit(SubmitRequest::new(EP_BULK_IN, Direction::In, 64))
        .unwrap();
    eventually("submit reaches server", || server.log.submits().len() == 1).await;
    session.unlink(seqnum);
    assert_eq!(reply.await, Err(TransferError::Unlinked(-104)));
    assert_eq!(session.pending(), 0);
    let unlinks = server.log.unlinks();
    assert_eq!(unlinks.len(), 1);
    assert_eq!(unlinks[0].1, seqnum);
    assert!(unlinks[0].2);
    assert_ne!(unlinks[0].0, seqnum, "unlink has its own seqnum");

    let ret = run(&session, bulk_out(vec![5; 3])).await.unwrap();
    assert_eq!(ret.actual_length, 3);
    let ret = run(&session, SubmitRequest::new(EP_BULK_IN, Direction::In, 64))
        .await
        .unwrap();
    assert_eq!(ret.data, vec![5; 3], "session still usable after unlink");
    server.assert_clean();
}

#[tokio::test]
async fn unlink_after_completion_is_harmless() {
    let (server, _dev, session) = loopback().await;
    let (seqnum, reply) = session.submit(control_in(REQ_PATTERN, 0, 4)).unwrap();
    assert!(reply.await.is_ok());
    session.unlink(seqnum);
    session.unlink(12345);
    assert!(server.log.unlinks().is_empty(), "nothing to unlink");
    run(&session, control_in(REQ_PATTERN, 0, 4)).await.unwrap();
}

#[tokio::test]
async fn racing_unlink_and_completion() {
    // The completion may win (RET_SUBMIT then RET_UNLINK status 0) or the unlink may (RET_UNLINK
    // -ECONNRESET only). Every transfer must resolve exactly once either way.
    let racy = |urb: Urb| async move {
        tokio::time::sleep(Duration::from_micros(u64::from(urb.setup[2]) * 20)).await;
        Outcome::Complete(Completion::ok(vec![urb.setup[2]]))
    };
    let server = Server::start(vec![Export::new("1-1", racy)]).await;
    let session = client::import(server.connect().await, "1-1").await.unwrap();
    let mut outcomes = (0, 0);
    for i in 0..200u32 {
        let mut req = SubmitRequest::new(0, Direction::In, 1);
        req.setup[2] = (i % 50) as u8;
        let (seqnum, reply) = session.submit(req).unwrap();
        tokio::time::sleep(Duration::from_micros(u64::from(i % 7) * 100)).await;
        session.unlink(seqnum);
        match tokio::time::timeout(Duration::from_secs(5), reply)
            .await
            .expect("hung")
        {
            Ok(ret) => {
                assert_eq!(ret.data, vec![(i % 50) as u8]);
                outcomes.0 += 1;
            }
            Err(TransferError::Unlinked(_)) => outcomes.1 += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(session.pending(), 0);
    assert!(!session.is_closed());
    server.assert_clean();
    println!("completed {} unlinked {}", outcomes.0, outcomes.1);
}

#[tokio::test]
async fn unlink_all_cancels_everything() {
    let (server, _dev, session) = loopback().await;
    let replies: Vec<_> = (0..10)
        .map(|_| {
            session
                .submit(SubmitRequest::new(EP_BULK_IN, Direction::In, 8))
                .unwrap()
                .1
        })
        .collect();
    eventually("submits", || server.log.submits().len() == 10).await;
    session.unlink_all();
    session.unlink_all();
    for r in replies {
        assert!(matches!(r.await, Err(TransferError::Unlinked(_))));
    }
    assert_eq!(
        server.log.unlinks().len(),
        10,
        "each transfer is unlinked once"
    );
}

#[tokio::test]
async fn server_disconnect_fails_pending_and_future_transfers() {
    let (_server, _dev, session) = loopback().await;
    let waiting = session
        .submit(SubmitRequest::new(EP_BULK_IN, Direction::In, 8))
        .unwrap()
        .1;
    let mut req = SubmitRequest::new(0, Direction::Out, 0);
    req.setup = support::setup(0x40, REQ_DISCONNECT, 0, 0, 0);
    assert!(matches!(
        run(&session, req).await,
        Err(TransferError::Disconnected(_))
    ));
    assert!(matches!(waiting.await, Err(TransferError::Disconnected(_))));
    let reason = tokio::time::timeout(Duration::from_secs(5), session.closed())
        .await
        .unwrap();
    assert!(reason.contains("closed"), "{reason}");
    assert!(session.is_closed());
    assert!(matches!(
        session.submit(SubmitRequest::new(EP_BULK_IN, Direction::In, 8)),
        Err(TransferError::Disconnected(_))
    ));
    session.unlink_all();
}

#[tokio::test]
async fn client_close_resolves_everything() {
    let (server, _dev, session) = loopback().await;
    let waiting = session
        .submit(SubmitRequest::new(EP_BULK_IN, Direction::In, 8))
        .unwrap()
        .1;
    session.close();
    assert!(matches!(waiting.await, Err(TransferError::Disconnected(_))));
    eventually("server sees disconnect", || {
        server.log.events().contains(&Event::Disconnected)
    })
    .await;
}

async fn session_with_raw_reply(raw: Vec<u8>) -> (Server, Session) {
    let raw = Arc::new(raw);
    let device = move |_urb: Urb| {
        let raw = Arc::clone(&raw);
        async move { Outcome::Raw((*raw).clone()) }
    };
    let server = Server::start(vec![Export::new("1-1", device)]).await;
    let session = client::import(server.connect().await, "1-1").await.unwrap();
    (server, session)
}

fn header(words: &[u32]) -> Vec<u8> {
    let mut out: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    out.resize(48, 0);
    out
}

#[tokio::test]
async fn reply_for_unknown_seqnum_closes_session() {
    let (_server, session) = session_with_raw_reply(header(&[3, 999_999, 0, 0, 0, 0, 0])).await;
    let result = run(&session, SubmitRequest::new(0x81, Direction::In, 8)).await;
    assert!(
        matches!(result, Err(TransferError::Disconnected(ref r)) if r.contains("unknown seqnum")),
        "{result:?}"
    );
}

#[tokio::test]
async fn garbage_from_server_closes_session() {
    let (_server, session) = session_with_raw_reply(header(&[0xdead, 1])).await;
    let result = run(&session, SubmitRequest::new(0x81, Direction::In, 8)).await;
    assert!(
        matches!(result, Err(TransferError::Disconnected(ref r)) if r.contains("unexpected command")),
        "{result:?}"
    );
}

#[tokio::test]
async fn oversized_reply_closes_session() {
    let (_server, session) = session_with_raw_reply(header(&[3, 1, 0, 0, 0, 0, 1 << 20])).await;
    let result = run(&session, SubmitRequest::new(0x81, Direction::In, 8)).await;
    assert!(
        matches!(result, Err(TransferError::Disconnected(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn truncated_reply_closes_session() {
    let mut raw = header(&[3, 1, 0, 0, 0, 0, 8]);
    raw.extend([1, 2, 3]);
    let raw = Arc::new(raw);
    let count = Arc::new(AtomicU32::new(0));
    let device = move |_urb: Urb| {
        let raw = Arc::clone(&raw);
        let count = Arc::clone(&count);
        async move {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                Outcome::Raw((*raw).clone())
            } else {
                Outcome::Disconnect
            }
        }
    };
    let server = Server::start(vec![Export::new("1-1", device)]).await;
    let session = client::import(server.connect().await, "1-1").await.unwrap();
    let first = session
        .submit(SubmitRequest::new(0x81, Direction::In, 8))
        .unwrap()
        .1;
    let second = session
        .submit(SubmitRequest::new(0x81, Direction::In, 8))
        .unwrap()
        .1;
    assert!(matches!(first.await, Err(TransferError::Disconnected(_))));
    assert!(matches!(second.await, Err(TransferError::Disconnected(_))));
}

#[tokio::test]
async fn late_ret_submit_after_unlink_is_tolerated() {
    // Some servers answer an unlink with RET_UNLINK and still deliver the RET_SUBMIT.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut s, _) = listener.accept().await.unwrap();
        let mut req = [0u8; 40];
        s.read_exact(&mut req).await.unwrap();
        let mut out = vec![0x01, 0x11, 0x00, 0x03, 0, 0, 0, 0];
        let mut dev = vec![0u8; 312];
        dev[256..259].copy_from_slice(b"1-1");
        out.extend(dev);
        s.write_all(&out).await.unwrap();
        let mut submit = [0u8; 48];
        s.read_exact(&mut submit).await.unwrap();
        let mut unlink = [0u8; 48];
        s.read_exact(&mut unlink).await.unwrap();
        let seq = u32::from_be_bytes(submit[4..8].try_into().unwrap());
        let unlink_seq = u32::from_be_bytes(unlink[4..8].try_into().unwrap());
        s.write_all(&header(&[4, unlink_seq, 0, 0, 0, (-104i32) as u32]))
            .await
            .unwrap();
        let mut late = header(&[3, seq, 0, 0, 0, 0, 2]);
        late.extend([1, 2]);
        s.write_all(&late).await.unwrap();
        let mut next = [0u8; 48];
        s.read_exact(&mut next).await.unwrap();
        let seq = u32::from_be_bytes(next[4..8].try_into().unwrap());
        let mut ok = header(&[3, seq, 0, 0, 0, 0, 1]);
        ok.push(42);
        s.write_all(&ok).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let session = client::import(tokio::net::TcpStream::connect(addr).await.unwrap(), "1-1")
        .await
        .unwrap();
    let (seq, reply) = session
        .submit(SubmitRequest::new(0x81, Direction::In, 2))
        .unwrap();
    session.unlink(seq);
    assert_eq!(reply.await, Err(TransferError::Unlinked(-104)));
    let ret = run(&session, SubmitRequest::new(0x81, Direction::In, 1))
        .await
        .unwrap();
    assert_eq!(ret.data, vec![42]);
    assert!(!session.is_closed());
}

#[tokio::test]
async fn dropping_reply_future_does_not_break_session() {
    let (server, _dev, session) = loopback().await;
    for _ in 0..20 {
        let (_, reply) = session.submit(control_in(REQ_PATTERN, 0, 8)).unwrap();
        drop(reply);
    }
    eventually("all replies drained", || session.pending() == 0).await;
    run(&session, control_in(REQ_PATTERN, 0, 8)).await.unwrap();
    server.assert_clean();
}

#[tokio::test]
async fn iso_transfers() {
    let (server, dev, session) = loopback().await;
    let mut req = SubmitRequest::new(EP_ISO_IN, Direction::In, 48);
    req.transfer_flags = usbip_macos::protocol::URB_ISO_ASAP;
    req.interval = 1;
    req.iso_packets = vec![usbip_macos::protocol::IsoPacketDescriptor {
        offset: 0,
        length: 48,
        actual_length: 0,
        status: 0,
    }];
    let ret = run(&session, req).await.unwrap();
    assert_eq!(ret.data, pattern(48, 0));
    assert_eq!(ret.iso_packets[0].actual_length, 48);

    let mut req = SubmitRequest::new(EP_ISO_OUT, Direction::Out, 16);
    req.interval = 1;
    req.data = vec![3; 16];
    req.iso_packets = vec![usbip_macos::protocol::IsoPacketDescriptor {
        offset: 0,
        length: 16,
        actual_length: 0,
        status: 0,
    }];
    run(&session, req).await.unwrap();
    assert_eq!(dev.iso_out(), vec![3; 16]);
    let urb = &server.log.submits()[1];
    assert_eq!(urb.number_of_packets, 1);
    assert_eq!(urb.iso.len(), 1);
    server.assert_clean();
}

#[tokio::test]
async fn works_over_in_memory_streams() {
    // The session is generic over the transport; a duplex pipe makes byte-exact tests possible.
    let (client_end, mut server_end) = tokio::io::duplex(4096);
    let device = usbip_macos::protocol::ExportedDevice {
        path: String::new(),
        busid: "1-1".into(),
        busnum: 1,
        devnum: 2,
        speed: usbip_macos::protocol::Speed::Full,
        id_vendor: 0,
        id_product: 0,
        bcd_device: 0,
        device_class: 0,
        device_subclass: 0,
        device_protocol: 0,
        configuration_value: 0,
        num_configurations: 0,
        num_interfaces: 0,
        interfaces: vec![],
    };
    let session = Session::start(client_end, device);
    let (seq, reply) = session
        .submit(SubmitRequest::new(0x82, Direction::In, 4))
        .unwrap();
    let mut frame = [0u8; 48];
    tokio::io::AsyncReadExt::read_exact(&mut server_end, &mut frame)
        .await
        .unwrap();
    assert_eq!(&frame[..20], &header(&[1, seq, 0x10002, 1, 2])[..20]);
    let mut ret = header(&[3, seq, 0, 0, 0, 0, 4]);
    ret.extend([9, 8, 7, 6]);
    server_end.write_all(&ret).await.unwrap();
    assert_eq!(reply.await.unwrap().data, vec![9, 8, 7, 6]);
}

#[tokio::test]
async fn simulated_class_devices_answer_standard_requests() {
    use support::classes::*;
    let fifo_full = fifo(FULL);
    let fifo_super = fifo(SUPER);
    let mouse = low_speed_mouse();
    let server = Server::start(vec![
        fifo_full.export("1-1", FULL),
        fifo_super.export("1-2", SUPER),
        mouse.export("1-3", LOW),
    ])
    .await;
    for (busid, mps0, bcd) in [("1-1", 64, 0x0200), ("1-2", 9, 0x0320), ("1-3", 8, 0x0110)] {
        let session = client::import(server.connect().await, busid).await.unwrap();
        let mut req = SubmitRequest::new(0, Direction::In, 18);
        req.setup = support::setup(0x80, 6, 0x0100, 0, 18);
        let d = run(&session, req).await.unwrap().data;
        assert_eq!(
            (d[7], u16::from_le_bytes([d[2], d[3]])),
            (mps0, bcd),
            "{busid}"
        );
        let mut req = SubmitRequest::new(0, Direction::In, 255);
        req.setup = support::setup(0x80, 6, 0x0200, 0, 255);
        let c = run(&session, req).await.unwrap().data;
        assert_eq!(
            u16::from_le_bytes([c[2], c[3]]) as usize,
            c.len(),
            "{busid} wTotalLength"
        );
        session.close();
    }
    let session = client::import(server.connect().await, "1-2").await.unwrap();
    let mut req = SubmitRequest::new(0, Direction::In, 5);
    req.setup = support::setup(0x80, 6, 0x0f00, 0, 5);
    assert_eq!(
        run(&session, req).await.unwrap().data[1],
        0x0f,
        "SuperSpeed devices have a BOS"
    );
    let mut out = SubmitRequest::new(0x01, Direction::Out, 3000);
    out.data = vec![9; 3000];
    run(&session, out).await.unwrap();
    let back = run(&session, SubmitRequest::new(0x81, Direction::In, 4096))
        .await
        .unwrap();
    assert_eq!(back.data, vec![9; 3000]);
    server.assert_clean();
}
