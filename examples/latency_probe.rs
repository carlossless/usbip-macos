//! Times small round trips, the pattern of debug probes and other request/response devices:
//! `latency_probe <vid> <pid> [count]`. Needs a bulk loopback on 0x01/0x81.

use std::time::{Duration, Instant};

use nusb::{
    transfer::{Buffer, Bulk, ControlIn, ControlType, In, Out, Recipient},
    MaybeFuture,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u16::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let (vid, pid) = (hex(&args[1]), hex(&args[2]));
    let count: u32 = args.get(3).map_or(500, |c| c.parse().unwrap());
    let info = nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == vid && d.product_id() == pid)
        .expect("device not found");
    let dev = info.open().wait().unwrap();
    let timeout = Duration::from_secs(5);

    let start = Instant::now();
    for _ in 0..count {
        let status = ControlIn {
            control_type: ControlType::Standard,
            recipient: Recipient::Device,
            request: 0x00,
            value: 0,
            index: 0,
            length: 2,
        };
        dev.control_in(status, timeout).wait().expect("GET_STATUS");
    }
    let control = start.elapsed() / count;

    let iface = dev.detach_and_claim_interface(0).wait().unwrap();
    let mut out = iface.endpoint::<Bulk, Out>(0x01).unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(0x81).unwrap();
    let start = Instant::now();
    for i in 0..count {
        let mut buf = Buffer::new(63);
        buf.extend_from_slice(&[i as u8; 63]);
        out.transfer_blocking(buf, timeout).status.expect("OUT");
        let c = inp.transfer_blocking(Buffer::new(512), timeout);
        c.status.expect("IN");
        assert_eq!(c.actual_len, 63);
    }
    let bulk = start.elapsed() / count;
    println!(
        "control {:.2} ms, bulk round trip {:.2} ms ({count} each)",
        control.as_secs_f64() * 1e3,
        bulk.as_secs_f64() * 1e3
    );
}
