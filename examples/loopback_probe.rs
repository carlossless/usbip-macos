//! Pushes data through a bulk loopback interface: `loopback_probe <vid> <pid> [rounds]`.

use std::time::Duration;

use nusb::{
    descriptors::TransferType,
    transfer::{Buffer, Bulk, Direction, In, Out},
    MaybeFuture,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let vid = u16::from_str_radix(&args[1], 16).unwrap();
    let pid = u16::from_str_radix(&args[2], 16).unwrap();
    let rounds: usize = args.get(3).map_or(20, |r| r.parse().unwrap());
    let info = nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == vid && d.product_id() == pid)
        .expect("device not found");
    let dev = info.open().wait().unwrap();
    let config = dev.active_configuration().unwrap();
    let alt = config
        .interface_alt_settings()
        .find(|a| {
            a.endpoints()
                .any(|e| e.transfer_type() == TransferType::Bulk)
        })
        .expect("bulk interface");
    let find = |d| {
        alt.endpoints()
            .find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == d)
            .unwrap()
    };
    let (ep_in, ep_out) = (find(Direction::In), find(Direction::Out));
    let iface = dev
        .detach_and_claim_interface(alt.interface_number())
        .wait()
        .unwrap();
    let mut out = iface.endpoint::<Bulk, Out>(ep_out.address()).unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(ep_in.address()).unwrap();
    let timeout = Duration::from_secs(5);
    for round in 0..rounds {
        let size = 1 + (round * 37) % 4096;
        // A whole number of packets would need a zero-length packet to end the transfer.
        let size = if size % 512 == 0 { size - 1 } else { size };
        let payload: Vec<u8> = (0..size).map(|i| (i + round) as u8).collect();
        let mut buf = Buffer::new(size);
        buf.extend_from_slice(&payload);
        out.transfer_blocking(buf, timeout).status.expect("OUT");
        let mut got = Vec::new();
        while got.len() < size {
            let c = inp.transfer_blocking(Buffer::new(4096), timeout);
            if let Err(e) = c.status {
                panic!(
                    "round {round} ({size} bytes): IN failed after {} bytes: {e:?}",
                    got.len()
                );
            }
            got.extend_from_slice(&c.buffer[..c.actual_len]);
        }
        assert_eq!(got, payload, "round {round} ({size} bytes)");
    }
    println!("{rounds} rounds ok");
}
