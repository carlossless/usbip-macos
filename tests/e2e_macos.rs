//! Attaches the simulated loopback device through the real binary and drives it from the macOS
//! side with nusb. Needs root and SIP relaxed for IOUSBHostControllerInterface, so it only runs
//! when `USBIP_E2E=1` is set (the cargo runner already uses `sudo -E` on macOS).
#![cfg(target_os = "macos")]

mod support;

use std::time::{Duration, Instant};

use nusb::{
    transfer::{
        Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient,
        TransferError,
    },
    DeviceInfo, MaybeFuture,
};
use support::{
    device::*,
    macos::{enabled, find_device as find, wait_for, Client, TIMEOUT},
    server::{Event, Server},
};

fn find_device() -> Option<DeviceInfo> {
    find(VID, PID)
}

struct Attached {
    // Declared first so the client is stopped before the server and runtime go away.
    child: Client,
    server: Option<Server>,
    device: LoopbackDevice,
    info: DeviceInfo,
    _rt: tokio::runtime::Runtime,
}

impl Attached {
    fn new() -> Self {
        wait_for(
            "previous device to go away",
            Duration::from_secs(10),
            || find_device().is_none(),
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let device = LoopbackDevice::default();
        let server = rt.block_on(Server::start(vec![device.export("1-1")]));
        let mut child = Client::attach(&server.address(), "1-1");
        let info = child.device(VID, PID);
        Self {
            child,
            server: Some(server),
            device,
            info,
            _rt: rt,
        }
    }
    fn server(&self) -> &Server {
        self.server.as_ref().unwrap()
    }

    fn open(&self) -> nusb::Device {
        self.info.open().wait().expect("open device")
    }
}

macro_rules! e2e {
    () => {
        let Some(_guard) = enabled("USBIP_E2E") else {
            return;
        };
    };
}

fn vendor_in(request: u8, value: u16, length: u16) -> ControlIn {
    ControlIn {
        control_type: ControlType::Vendor,
        recipient: Recipient::Device,
        request,
        value,
        index: 0,
        length,
    }
}

#[test]
fn enumerates_with_descriptors_and_strings() {
    e2e!();
    let a = Attached::new();
    assert_eq!(a.info.manufacturer_string(), Some(MANUFACTURER));
    assert_eq!(a.info.product_string(), Some(PRODUCT));
    assert_eq!(a.info.serial_number(), Some(SERIAL));
    let dev = a.open();
    let config = dev.active_configuration().unwrap();
    assert_eq!(config.configuration_value(), 1);
    assert_eq!(config.interfaces().count(), 2);
    assert_eq!(dev.device_descriptor().max_packet_size_0(), 64);
    let _iface = dev.claim_interface(0).wait().unwrap();
    wait_for("the device to be configured", TIMEOUT, || {
        a.device.configuration() == 1
    });
    a.server().assert_clean();
}

#[test]
fn control_transfers() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    let data = dev
        .control_in(vendor_in(REQ_PATTERN, 5, 200), TIMEOUT)
        .wait()
        .unwrap();
    assert_eq!(data, pattern(200, 5));

    dev.control_out(
        ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: REQ_WRITE,
            value: 0,
            index: 0,
            data: b"round trip",
        },
        TIMEOUT,
    )
    .wait()
    .unwrap();
    assert_eq!(
        dev.control_in(vendor_in(REQ_READ, 0, 64), TIMEOUT)
            .wait()
            .unwrap(),
        b"round trip"
    );

    let short = dev
        .control_in(vendor_in(REQ_SHORT, 3, 64), TIMEOUT)
        .wait()
        .unwrap();
    assert_eq!(short.len(), 3, "short control reads report the real length");

    let err = dev
        .control_in(vendor_in(REQ_STALL, 0, 8), TIMEOUT)
        .wait()
        .unwrap_err();
    assert_eq!(err, TransferError::Stall);
    let data = dev
        .control_in(vendor_in(REQ_PATTERN, 1, 8), TIMEOUT)
        .wait()
        .unwrap();
    assert_eq!(
        data,
        pattern(8, 1),
        "control endpoint recovers after a stall"
    );

    for i in 0..200u16 {
        let data = dev
            .control_in(vendor_in(REQ_PATTERN, i, 1 + i % 64), TIMEOUT)
            .wait()
            .unwrap();
        assert_eq!(data, pattern(1 + usize::from(i % 64), i as u8));
    }
    a.server().assert_clean();
}

#[test]
fn bulk_loopback() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut out = iface.endpoint::<Bulk, Out>(EP_BULK_OUT).unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(EP_BULK_IN).unwrap();
    for size in [1usize, 63, 64, 511, 512, 513, 4096, 65_536, 1 << 20] {
        let payload = pattern(size, size as u8);
        let mut buf = Buffer::new(size);
        buf.extend_from_slice(&payload);
        out.transfer_blocking(buf, TIMEOUT).status.unwrap();
        let mut received = Vec::new();
        while received.len() < size {
            let c = inp.transfer_blocking(Buffer::new(64 * 1024), TIMEOUT);
            c.status.unwrap();
            received.extend_from_slice(&c.buffer[..c.actual_len]);
        }
        assert_eq!(received.len(), size);
        assert!(received == payload, "payload mismatch for {size} bytes");
    }
    a.server().assert_clean();
}

#[test]
fn many_queued_bulk_transfers() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut out = iface.endpoint::<Bulk, Out>(EP_BULK_OUT).unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(EP_BULK_IN).unwrap();
    for i in 0..64u8 {
        let mut buf = Buffer::new(512);
        buf.extend_from_slice(&[i; 512]);
        out.submit(buf);
    }
    for _ in 0..64 {
        out.wait_next_complete(TIMEOUT)
            .expect("OUT completion")
            .status
            .unwrap();
    }
    for _ in 0..64 {
        inp.submit(Buffer::new(512));
    }
    for i in 0..64u8 {
        let c = inp.wait_next_complete(TIMEOUT).expect("IN completion");
        c.status.unwrap();
        assert_eq!(
            &c.buffer[..c.actual_len],
            &[i; 512][..],
            "transfers completed out of order"
        );
    }
    a.server().assert_clean();
}

#[test]
fn interrupt_endpoint() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut int = iface.endpoint::<Interrupt, In>(EP_INT_IN).unwrap();
    let mut last = 0;
    for _ in 0..20 {
        let c = int.transfer_blocking(Buffer::new(64), TIMEOUT);
        c.status.unwrap();
        let n = u64::from_le_bytes(c.buffer[..8].try_into().unwrap());
        assert!(n > last);
        last = n;
    }
}

#[test]
fn cancelling_a_pending_transfer() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(EP_BULK_IN).unwrap();
    inp.submit(Buffer::new(512));
    let server = a.server();
    wait_for("IN reaches server", TIMEOUT, || {
        server.log.submits().iter().any(|u| u.ep == 1 && u.is_in())
    });
    inp.cancel_all();
    let c = inp.wait_next_complete(TIMEOUT).expect("cancel completes");
    assert_eq!(c.status, Err(TransferError::Cancelled));
    wait_for("unlink", TIMEOUT, || {
        server.log.unlinks().iter().any(|u| u.2)
    });

    let mut out = iface.endpoint::<Bulk, Out>(EP_BULK_OUT).unwrap();
    let mut buf = Buffer::new(4);
    buf.extend_from_slice(b"next");
    out.transfer_blocking(buf, TIMEOUT).status.unwrap();
    let c = inp.transfer_blocking(Buffer::new(512), TIMEOUT);
    c.status.unwrap();
    assert_eq!(
        &c.buffer[..c.actual_len],
        b"next",
        "the cancelled transfer did not eat data"
    );
    server.assert_clean();
}

#[test]
fn halt_and_clear_halt() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut out = iface.endpoint::<Bulk, Out>(EP_BULK_OUT).unwrap();
    dev.control_out(
        ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: REQ_HALT,
            value: u16::from(EP_BULK_OUT),
            index: 0,
            data: &[],
        },
        TIMEOUT,
    )
    .wait()
    .unwrap();
    let mut buf = Buffer::new(4);
    buf.extend_from_slice(&[1; 4]);
    assert_eq!(
        out.transfer_blocking(buf, TIMEOUT).status,
        Err(TransferError::Stall)
    );
    out.clear_halt().wait().unwrap();
    let mut buf = Buffer::new(4);
    buf.extend_from_slice(&[1; 4]);
    out.transfer_blocking(buf, TIMEOUT).status.unwrap();
}

#[test]
fn alternate_settings() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    let iface = dev.claim_interface(1).wait().unwrap();
    iface.set_alt_setting(1).wait().unwrap();
    iface.set_alt_setting(0).wait().unwrap();
    let alt_requests = a
        .server()
        .log
        .submits()
        .iter()
        .filter(|u| u.ep == 0 && u.request_type() == 0x01 && u.request() == 11)
        .count();
    assert!(alt_requests >= 2);
}

#[test]
fn sigterm_detaches_cleanly() {
    e2e!();
    let mut a = Attached::new();
    let dev = a.open();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(EP_BULK_IN).unwrap();
    inp.submit(Buffer::new(512));
    let server = a.server.take().unwrap();
    wait_for("IN reaches server", TIMEOUT, || {
        server.log.submits().iter().any(|u| u.ep == 1)
    });
    let status = a.child.terminate();
    assert!(status.success(), "{status}");
    assert!(
        server.log.unlinks().iter().any(|u| u.2),
        "pending transfer unlinked on exit"
    );
    wait_for("server sees disconnect", TIMEOUT, || {
        server.log.events().contains(&Event::Disconnected)
    });
    wait_for("device removed", Duration::from_secs(10), || {
        find_device().is_none()
    });
    let c = inp
        .wait_next_complete(TIMEOUT)
        .expect("pending transfer completes");
    assert!(c.status.is_err());
    server.assert_clean();
}

#[test]
fn server_disappearing_exits_with_error() {
    e2e!();
    let mut a = Attached::new();
    drop(a.server.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = a.child.try_wait() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "client kept running without a server"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    wait_for("device removed", Duration::from_secs(10), || {
        find_device().is_none()
    });
}

#[test]
fn host_reset_resets_the_device() {
    e2e!();
    let a = Attached::new();
    let dev = a.open();
    dev.reset().wait().expect("reset");
    drop(dev);
    wait_for("reset reaches the server", Duration::from_secs(10), || {
        a.server().log.events().contains(&Event::Reset)
    });
    let mut info = None;
    wait_for(
        "device back after the reset",
        Duration::from_secs(10),
        || {
            info = find_device();
            info.is_some()
        },
    );
    let dev = info.unwrap().open().wait().expect("reopen after reset");
    assert_eq!(
        dev.control_in(vendor_in(REQ_PATTERN, 3, 8), TIMEOUT)
            .wait()
            .unwrap(),
        pattern(8, 3)
    );
    assert_eq!(a.device.configuration(), 1, "macOS configured it again");
    a.server().assert_clean();
}

#[test]
fn reattach_after_detach() {
    e2e!();
    for _ in 0..3 {
        let mut a = Attached::new();
        let dev = a.open();
        assert_eq!(
            dev.control_in(vendor_in(REQ_PATTERN, 2, 4), TIMEOUT)
                .wait()
                .unwrap(),
            pattern(4, 2)
        );
        drop(dev);
        assert!(a.child.terminate().success());
    }
}

fn attach_simulated<F: support::usb::Function>(
    device: &support::usb::StdDevice<F>,
    speed: u32,
) -> (tokio::runtime::Runtime, Server, Client, DeviceInfo) {
    let (vid, pid) = (device.descriptors.vid(), device.descriptors.pid());
    support::macos::wait_gone(vid, pid);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(Server::start(vec![device.export("1-1", speed)]));
    let mut client = Client::attach(&server.address(), "1-1");
    let info = client.device(vid, pid);
    (rt, server, client, info)
}

fn fifo_round_trips(info: &DeviceInfo, max_packet: usize) {
    let dev = info.open().wait().unwrap();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut out = iface.endpoint::<Bulk, Out>(0x01).unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(0x81).unwrap();
    assert_eq!(inp.max_packet_size(), max_packet);
    for size in [
        1,
        max_packet - 1,
        max_packet + 1,
        10 * max_packet + 7,
        200_001,
    ] {
        let payload = pattern(size, size as u8);
        let mut buf = Buffer::new(size);
        buf.extend_from_slice(&payload);
        out.transfer_blocking(buf, TIMEOUT).status.unwrap();
        let mut got = Vec::new();
        while got.len() < size {
            let c = inp.transfer_blocking(Buffer::new(64 * max_packet), TIMEOUT);
            c.status.unwrap();
            got.extend_from_slice(&c.buffer[..c.actual_len]);
        }
        assert!(got == payload, "{size} bytes differ");
    }
}

#[test]
fn full_speed_device() {
    e2e!();
    let device = support::classes::fifo(support::classes::FULL);
    let (_rt, server, _client, info) = attach_simulated(&device, support::classes::FULL);
    assert_eq!(info.speed(), Some(nusb::Speed::Full));
    fifo_round_trips(&info, 64);
    server.assert_clean();
}

#[test]
fn superspeed_device() {
    e2e!();
    let device = support::classes::fifo(support::classes::SUPER);
    let (_rt, server, _client, info) = attach_simulated(&device, support::classes::SUPER);
    assert_eq!(info.speed(), Some(nusb::Speed::Super));
    fifo_round_trips(&info, 1024);
    server.assert_clean();
}

#[test]
fn superspeed_plus_device() {
    e2e!();
    let device = support::classes::fifo(support::classes::SUPER_PLUS);
    let (_rt, server, _client, info) = attach_simulated(&device, support::classes::SUPER_PLUS);
    assert_eq!(info.speed(), Some(nusb::Speed::SuperPlus));
    fifo_round_trips(&info, 1024);
    server.assert_clean();
}

#[test]
fn high_bandwidth_interrupt_endpoint() {
    e2e!();
    let device = support::classes::high_bandwidth_interrupt();
    let (_rt, server, _client, info) = attach_simulated(&device, support::classes::HIGH);
    let dev = info.open().wait().unwrap();
    let iface = dev.claim_interface(0).wait().unwrap();
    let mut inp = iface.endpoint::<Interrupt, In>(0x81).unwrap();
    for _ in 0..20 {
        let c = inp.transfer_blocking(Buffer::new(3072), TIMEOUT);
        c.status.unwrap();
        assert_eq!(
            c.actual_len, 3072,
            "all three packets of the microframe arrive"
        );
        let first = c.buffer[0];
        assert!(c.buffer[..3072]
            .iter()
            .enumerate()
            .all(|(i, b)| *b == first.wrapping_add(i as u8)));
    }
    assert!(server
        .log
        .submits()
        .iter()
        .filter(|u| u.ep == 1)
        .all(|u| u.length == 3072 && u.interval == 1));
    server.assert_clean();
}

#[test]
fn low_speed_mouse() {
    e2e!();
    let device = support::classes::low_speed_mouse();
    let (_rt, server, _client, info) = attach_simulated(&device, support::classes::LOW);
    assert_eq!(info.speed(), Some(nusb::Speed::Low));
    wait_for("macOS to poll the mouse", Duration::from_secs(10), || {
        *device.function.polls.lock().unwrap() > 10
    });
    let hid = support::macos::sh("hidutil list");
    assert!(
        hid.lines()
            .any(|l| l.contains("0x1209") && l.contains("0x7359")),
        "{hid}"
    );
    server.assert_clean();
}

#[test]
fn switching_configurations() {
    e2e!();
    let device = support::classes::two_configurations();
    let (_rt, server, _client, info) = attach_simulated(&device, support::classes::HIGH);
    let dev = info.open().wait().unwrap();
    assert_eq!(dev.configurations().count(), 2);
    dev.set_configuration(2).wait().unwrap();
    assert_eq!(
        device.configuration(),
        2,
        "SET_CONFIGURATION reached the device"
    );
    assert_eq!(dev.active_configuration().unwrap().configuration_value(), 2);
    dev.set_configuration(1).wait().unwrap();
    assert_eq!(device.configuration(), 1);
    fifo_round_trips(&info, 512);
    server.assert_clean();
}
