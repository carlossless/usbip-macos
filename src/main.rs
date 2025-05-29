use std::{cell::UnsafeCell, process, sync::Arc};

use controller_interface::{ControllerInterface, ForceableSend};
use dispatch2::dispatch_main;
use tokio;
use usbip::{UsbIpClient};

mod controller_interface;
mod device;
mod endpoint;

mod usbip;

// #[tokio::main]
fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

    let addr = "carlossless-chedar:3240";

    let mut client = UsbIpClient::new();
    rt.block_on(client.connect(addr)).unwrap();
    let devices = rt.block_on(client.list_devices()).unwrap();
    let device = devices.first().unwrap();

    rt.block_on(client.connect(addr)).unwrap();
    rt.block_on(client.import_device(*device.get_busid())).unwrap();
    let client = Arc::new(UnsafeCell::new(client));
    let clien_1 = ForceableSend(Arc::clone(&client));

    let con_iface = ControllerInterface::new(client);
    if let Err(e) = con_iface {
        eprintln!("Error initializing controller interface: {}", e);
        return;
    }

    rt.spawn(async move {
        println!("Controller interface initialized successfully.");
        let cl = clien_1;
        loop {
            // let mut guard = clien_1.lock().await;
            unsafe {
                // Get mutable reference to UsbIpClient and call poll if it exists
                let client_ref = &mut *cl.0.get();
                if let Err(error) =  client_ref.poll().await {
                    eprintln!("Error polling USB/IP client: {}", error);
                    process::exit(1);
                }
            }
            // drop(guard); // Explicitly drop the lock to avoid deadlocks
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    dispatch_main();
}
