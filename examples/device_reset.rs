//! Asks the host to reset a device and enumerate it again: `device_reset <vid> <pid>`.

use nusb::MaybeFuture;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let hex = |s: &str| u16::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
    let (vid, pid) = (hex(&args[1]), hex(&args[2]));
    let info = nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == vid && d.product_id() == pid)
        .expect("device not found");
    let dev = info.open().wait().unwrap();
    dev.reset().wait().expect("reset");
    println!("reset");
}
