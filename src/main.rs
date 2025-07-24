use std::{
    cell::UnsafeCell,
    process::{self},
    sync::Arc,
};

use clap::{arg, Command};
use clap_num::maybe_hex;
use controller_interface::{ControllerInterface, ForceableSend};
use dispatch2::dispatch_main;
use log::debug;
use simple_logger::SimpleLogger;
use tokio;
use usbip::UsbIpClient;

mod controller_interface;
mod device;
mod endpoint;

mod usbip;

// #[tokio::main]
fn main() {
    #[cfg(debug_assertions)]
    let default_log_level = log::LevelFilter::Debug;
    #[cfg(not(debug_assertions))]
    let default_log_level = log::LevelFilter::Off;

    SimpleLogger::new()
        .with_utc_timestamps()
        .with_level(default_log_level)
        .env()
        .init()
        .unwrap();

    let matches = Command::new("usbip-macos")
        .about("USBIP client for macOS systems")
        .version(env!("CARGO_PKG_VERSION"))
        .arg_required_else_help(true)
        .author("Karolis Stasaitis")
        .subcommand(
            Command::new("attach")
                .arg(
                    arg!(--busid <BUSID>)
                        .value_parser(clap::value_parser!(String))
                        .required_unless_present_all(&["vendor_id", "product_id"])
                        .conflicts_with_all(&["vendor_id", "product_id", "device"])
                        .help("The device to attach (busid)"),
                )
                .arg(
                    arg!(--device <DEVICE>)
                        .value_parser(clap::value_parser!(u32))
                        .required_unless_present_all(&["vendor_id", "product_id"])
                        .conflicts_with_all(&["vendor_id", "product_id", "busid"])
                        .conflicts_with("busid")
                        .help("id of the virtual UDC"),
                )
                .arg(
                    arg!(--vendor_id <VENDOR_ID>)
                        .value_parser(maybe_hex::<u16>)
                        .required_unless_present("busid")
                        .conflicts_with("busid")
                        .conflicts_with("device")
                        .help("The vendor ID of the device to attach"),
                )
                .arg(
                    arg!(--product_id <PRODUCT_ID>)
                        .value_parser(maybe_hex::<u16>)
                        .required_unless_present("busid")
                        .conflicts_with("busid")
                        .conflicts_with("device")
                        .help("The product ID of the device to attach"),
                ),
        )
        .subcommand(Command::new("list"))
        .arg(
            arg!(-r --remote <HOST>)
                .value_parser(clap::value_parser!(String))
                .required(true)
                .help("The host to connect to"),
        )
        .arg(
            arg!(-p --"tcp-port" <PORT>)
                .value_parser(clap::value_parser!(u16))
                .default_value("3240")
                .help("The port to connect to"),
        )
        .get_matches();

    let host = matches.get_one::<String>("remote").unwrap();
    let port = matches.get_one::<u16>("tcp-port").unwrap();

    let addr = format!("{host}:{port}");

    match matches.subcommand() {
        Some(("attach", submatches)) => {
            let busid = submatches.get_one::<String>("busid").cloned();
            let device = submatches.get_one::<u32>("device").cloned();
            let vendor_id = submatches.get_one::<u16>("vendor_id").cloned();
            let product_id = submatches.get_one::<u16>("product_id").cloned();

            attach(&addr, busid, device, vendor_id, product_id).unwrap_or_else(|e| {
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
    device: Option<u32>,
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
    let device = devices.iter().find(|d| {
        if let Some(busid) = &busid {
            d.get_busid() == busid.as_bytes()
        } else if let Some(device) = &device {
            d.get_devnum() == *device
        } else if let (Some(vendor_id), Some(product_id)) = (vendor_id, product_id) {
            d.get_id_vendor() == vendor_id && d.get_id_product() == product_id
        } else {
            false
        }
    });

    let Some(device) = device else {
        eprintln!("Device not found");
        process::exit(1);
    };

    rt.block_on(client.connect(&addr)).unwrap();
    rt.block_on(client.import_device(*device.get_busid()))
        .unwrap();
    let ci_client = Arc::new(UnsafeCell::new(client));
    let usbip_client = ForceableSend(Arc::clone(&ci_client));

    let _con_iface = ControllerInterface::new(ci_client)?;

    rt.spawn(async move {
        debug!("Controller interface initialized successfully.");
        let cl = usbip_client;
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
}
