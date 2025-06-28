use std::{
    cell::UnsafeCell,
    os::unix::thread,
    process::{self},
    sync::Arc,
    thread::sleep,
    time::Duration,
};

use clap::{arg, Command};
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
    let matches = Command::new("usbip-darwin")
        .about("USBIP client for darwin (macOS) systems")
        .version(env!("CARGO_PKG_VERSION"))
        .arg_required_else_help(true)
        .author("Karolis Stasaitis")
        .subcommand(Command::new("attach"))
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
        Some(("attach", _)) => {
            attach(&addr, 0).unwrap_or_else(|e| {
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

fn attach(addr: &str, busid: u32) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut client = UsbIpClient::new();
    rt.block_on(client.connect(&addr)).unwrap();
    let devices = rt.block_on(client.list_devices()).unwrap();
    let device = devices
        .iter()
        .find(
            |d| {
                (d.get_id_vendor() == 0x05ac && d.get_id_product() == 0x024f)
                    || (d.get_id_vendor() == 0x0603 && d.get_id_product() == 0x1020)
            }, // TODO: move into cli argument
        )
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
                sleep(Duration::from_millis(10));
            }
        }
    });

    dispatch_main();

    // Ok(())
}
