use std::{
    cell::UnsafeCell,
    process::{self},
    sync::Arc,
};

use clap::{arg, Command};
use clap_num::maybe_hex;
use controller_interface::{ControllerInterface, ForceableSend};
use dispatch2::dispatch_main;
use tokio;
use usbip::UsbIpClient;

mod controller_interface;
mod device;
mod endpoint;

mod usbip;

// #[tokio::main]
fn main() {
    let matches = Command::new("usbip-macos")
        .about("USBIP client for macOS systems")
        .version(env!("CARGO_PKG_VERSION"))
        .arg_required_else_help(true)
        .author("Karolis Stasaitis")
        .subcommand(
            Command::new("attach")
                .arg(
                    arg!(-b --busid <BUSID>)
                        .value_parser(clap::value_parser!(String))
                        .required_unless_present_all(&["vendor_id", "product_id"])
                        .conflicts_with_all(&["vendor_id", "product_id"])
                        .help("The device to attach (busid)"),
                )
                .arg(
                    arg!(--vendor_id <VENDOR_ID>)
                        .value_parser(maybe_hex::<u16>)
                        .required_unless_present("busid")
                        .conflicts_with("busid")
                        .help("The vendor ID of the device to attach"),
                )
                .arg(
                    arg!(--product_id <PRODUCT_ID>)
                        .value_parser(maybe_hex::<u16>)
                        .required_unless_present("busid")
                        .conflicts_with("busid")
                        .help("The product ID of the device to attach"),
                ),
        )
        .subcommand(Command::new("list"))
        .arg(
            arg!(--host <HOST>)
                .value_parser(clap::value_parser!(String))
                .required(true)
                .help("The host to connect to"),
        )
        .arg(
            arg!(-p --port <PORT>)
                .value_parser(clap::value_parser!(u16))
                .default_value("3240")
                .help("The port to connect to"),
        )
        .get_matches();

    let host = matches.get_one::<String>("host").unwrap();
    let port = matches.get_one::<u16>("port").unwrap();

    let addr = format!("{host}:{port}");

    match matches.subcommand() {
        Some(("attach", submatches)) => {
            let busid = submatches.get_one::<String>("busid").cloned();
            let vendor_id = submatches.get_one::<u16>("vendor_id").cloned();
            let product_id = submatches.get_one::<u16>("product_id").cloned();

            attach(&addr, busid, vendor_id, product_id).unwrap_or_else(|e| {
                eprintln!("Error attach device: {}", e);
                process::exit(1);
            });
        }
        Some(("list", _)) => {
            list(&addr).unwrap_or_else(|e| {
                eprintln!("Error listing devices: {}", e);
                process::exit(1);
            });
        }
        _ => {
            panic!("Unknown command or no command provided")
        }
    }
}

fn list(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut client = UsbIpClient::new();
    rt.block_on(client.connect(addr))?;
    let devices = rt.block_on(client.list_devices())?;

    for device in devices {
        println!("Device: {:?}", device);
    }

    Ok(())
}

fn attach(
    addr: &str,
    busid: Option<String>,
    vendor_id: Option<u16>,
    product_id: Option<u16>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut client = UsbIpClient::new();
    rt.block_on(client.connect(&addr)).unwrap();
    let devices = rt.block_on(client.list_devices()).unwrap();
    let device = devices
        .iter()
        .find(|d| {
            if let Some(busid) = &busid {
                d.get_busid() == busid.as_bytes()
            } else if let (Some(vendor_id), Some(product_id)) = (vendor_id, product_id) {
                d.get_id_vendor() == vendor_id && d.get_id_product() == product_id
            } else {
                false
            }
        })
        // .find(
        //     |d| {
        //         (d.get_id_vendor() == 0x05ac && d.get_id_product() == 0x024f)
        //             || (d.get_id_vendor() == 0x0603 && d.get_id_product() == 0x1020)
        //     }, // TODO: move into cli argument
        // )
        .unwrap();

    rt.block_on(client.connect(&addr)).unwrap();
    rt.block_on(client.import_device(*device.get_busid()))
        .unwrap();
    let client = Arc::new(UnsafeCell::new(client));
    let clien_1 = ForceableSend(Arc::clone(&client));

    let _con_iface = ControllerInterface::new(client)?;

    rt.spawn(async move {
        println!("Controller interface initialized successfully.");
        let cl = clien_1;
        loop {
            unsafe {
                // Get mutable reference to UsbIpClient and call poll if it exists
                let client_ref = &mut *cl.0.get();
                if let Err(error) = client_ref.poll().await {
                    eprintln!("Error polling USB/IP client: {}", error);
                    process::exit(1);
                }
            }
        }
    });

    dispatch_main();

    // Ok(())
}
