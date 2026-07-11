use std::{process, sync::Arc};

use clap::{arg, Command};
use clap_num::maybe_hex;
use controller_interface::ControllerInterface;
use dispatch2::dispatch_main;
use log::debug;
use simple_logger::SimpleLogger;
use std::io::{self, Write};
use tokio::signal;
use tokio::sync::Mutex;
use usbip::{UsbIpClient, UsbIpReader, UsbIpWriter};

mod controller_interface;
mod device;
mod endpoint;
mod usbip;

fn main() {
    init_logger();
    let matches = build_cli().get_matches();
    let (host, port) = extract_connection_info(&matches);
    let addr = format!("{host}:{port}");
    handle_subcommands(&matches, &addr);
}

fn init_logger() {
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
}

fn build_cli() -> Command {
    Command::new("usbip-macos")
        .about("USBIP client for macOS systems")
        .version(env!("CARGO_PKG_VERSION"))
        .arg_required_else_help(true)
        .author("Karolis Stasaitis")
        .subcommand(build_attach_command())
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
}

fn build_attach_command() -> Command {
    Command::new("attach")
        .arg(
            arg!(--busid <BUSID>)
                .value_parser(clap::value_parser!(String))
                .required_unless_present_all(["vendor_id", "product_id"])
                .conflicts_with_all(["vendor_id", "product_id", "device"])
                .help("The device to attach (busid)"),
        )
        .arg(
            arg!(--device <DEVICE>)
                .value_parser(clap::value_parser!(u32))
                .required_unless_present_all(["vendor_id", "product_id"])
                .conflicts_with_all(["vendor_id", "product_id", "busid"])
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
        )
        .arg(
            arg!(--"wait-for-enter")
                .action(clap::ArgAction::SetTrue)
                .help("Wait for Enter key before plugging in port (for debugging)"),
        )
}

fn extract_connection_info(matches: &clap::ArgMatches) -> (&str, u16) {
    let host = matches.get_one::<String>("remote").unwrap();
    let port = *matches.get_one::<u16>("tcp-port").unwrap();
    (host, port)
}

fn handle_subcommands(matches: &clap::ArgMatches, addr: &str) {
    match matches.subcommand() {
        Some(("attach", submatches)) => {
            let attach_config = extract_attach_config(submatches);
            attach(addr, attach_config);
        }
        Some(("list", _)) => {
            if let Err(e) = list(addr) {
                eprintln!("Error listing devices: {}", e);
                process::exit(1);
            }
        }
        _ => {
            eprintln!("Unknown command or no command provided");
            process::exit(1);
        }
    }
}

#[derive(Debug)]
struct AttachConfig {
    busid: Option<String>,
    device: Option<u32>,
    vendor_id: Option<u16>,
    product_id: Option<u16>,
    wait_for_enter: bool,
}

fn extract_attach_config(submatches: &clap::ArgMatches) -> AttachConfig {
    AttachConfig {
        busid: submatches.get_one::<String>("busid").cloned(),
        device: submatches.get_one::<u32>("device").cloned(),
        vendor_id: submatches.get_one::<u16>("vendor_id").cloned(),
        product_id: submatches.get_one::<u16>("product_id").cloned(),
        wait_for_enter: submatches.get_flag("wait-for-enter"),
    }
}

fn list(addr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let rt = create_runtime()?;
    let mut client = UsbIpClient::new();

    rt.block_on(client.connect(addr))?;
    let devices = rt.block_on(client.list_devices())?;

    for device in devices {
        println!("Device: {:?}", device);
    }

    Ok(())
}

fn attach(addr: &str, config: AttachConfig) -> ! {
    let rt = match create_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error creating runtime: {}", e);
            process::exit(1);
        }
    };

    let mut client = UsbIpClient::new();

    if let Err(e) = rt.block_on(client.connect(addr)) {
        eprintln!("Error connecting to server: {}", e);
        process::exit(1);
    }

    let devices = match rt.block_on(client.list_devices()) {
        Ok(devices) => devices,
        Err(e) => {
            eprintln!("Error listing devices: {}", e);
            process::exit(1);
        }
    };

    let device = match find_target_device(&devices, &config) {
        Some(device) => device,
        None => {
            eprintln!("Device not found");
            process::exit(1);
        }
    };

    if let Err(e) = rt.block_on(client.connect(addr)) {
        eprintln!("Error reconnecting to server: {}", e);
        process::exit(1);
    }

    if let Err(e) = rt.block_on(client.import_device(*device.get_busid())) {
        eprintln!("Error importing device: {}", e);
        process::exit(1);
    }

    let device_speed = client
        .get_imported_device_speed()
        .copied()
        .expect("Device speed should be available after import");

    let (writer, reader) = match client.split() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Error splitting client: {}", e);
            process::exit(1);
        }
    };

    let writer = Arc::new(Mutex::new(writer));
    let writer_for_ci = Arc::clone(&writer);
    let wait_for_enter = config.wait_for_enter;

    std::thread::spawn(move || {
        let (_con_iface, ready_rx) = match ControllerInterface::new(writer_for_ci, device_speed) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("Error creating controller interface: {}", e);
                process::exit(1);
            }
        };

        // Wait for port to be powered on and ready
        if let Err(e) = ready_rx.recv() {
            eprintln!("Error waiting for port ready signal: {}", e);
            process::exit(1);
        }

        if wait_for_enter {
            print!("Waiting for Enter to continue plugging in port...");
            io::stdout().flush().unwrap();
            let _ = io::stdin().read_line(&mut String::new());
        }

        let port = unsafe {
            _con_iface
                ._control_interface
                .getPortStateMachineForPort_error(1)
                .unwrap()
        };
        unsafe { port.setConnected(true) };
    });

    spawn_polling_task(&rt, reader, Arc::clone(&writer));
    dispatch_main()
}

fn create_runtime() -> Result<tokio::runtime::Runtime, std::io::Error> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}

fn find_target_device<'a>(
    devices: &'a [crate::usbip::Device],
    config: &AttachConfig,
) -> Option<&'a crate::usbip::Device> {
    devices.iter().find(|d| {
        if let Some(busid) = &config.busid {
            d.get_busid() == busid.as_bytes()
        } else if let Some(device) = &config.device {
            d.get_devnum() == *device
        } else if let (Some(vendor_id), Some(product_id)) = (config.vendor_id, config.product_id) {
            d.get_id_vendor() == vendor_id && d.get_id_product() == product_id
        } else {
            false
        }
    })
}

fn spawn_polling_task(
    rt: &tokio::runtime::Runtime,
    mut reader: UsbIpReader,
    writer: Arc<Mutex<UsbIpWriter>>,
) {
    // Spawn the polling task
    rt.spawn(async move {
        debug!("Controller interface initialized successfully.");
        loop {
            if let Err(error) = reader.poll().await {
                eprintln!("Error polling USB/IP client: {}", error);
                process::exit(1);
            }
        }
    });

    // Spawn the signal handler task
    rt.spawn(async move {
        let ctrl_c = signal::ctrl_c();

        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install SIGTERM handler")
                .recv()
                .await;
        };

        tokio::select! {
            _ = ctrl_c => {
                debug!("Received SIGINT, shutting down...");
            }
            _ = terminate => {
                debug!("Received SIGTERM, shutting down...");
            }
        }

        // Unlink all pending transactions before exiting
        eprintln!("\nShutting down, unlinking pending transactions...");
        let mut w = writer.lock().await;
        if let Err(e) = w.unlink_all_pending().await {
            eprintln!("Error unlinking pending transactions: {}", e);
        }

        process::exit(0);
    });
}
