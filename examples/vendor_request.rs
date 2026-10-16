//! Sends a vendor OUT control request to a device: `vendor_request <vid> <pid> <request> [value]`.

use std::time::Duration;

use nusb::{
    transfer::{ControlOut, ControlType, Recipient},
    MaybeFuture,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u16::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let (vid, pid) = (hex(&args[1]), hex(&args[2]));
    let request = hex(&args[3]) as u8;
    let value = args.get(4).map_or(0, |v| hex(v));
    let info = nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == vid && d.product_id() == pid)
        .expect("device not found");
    let dev = info.open().wait().unwrap();
    dev.control_out(
        ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request,
            value,
            index: 0,
            data: &[],
        },
        Duration::from_secs(2),
    )
    .wait()
    .expect("control request");
    println!("sent");
}
