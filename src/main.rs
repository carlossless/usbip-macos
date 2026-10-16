use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use simple_logger::SimpleLogger;
use tokio::net::TcpStream;
use usbip_macos::{
    cli::{self, Action, Selector},
    client,
};

fn main() -> ExitCode {
    init_logger();
    let args = cli::parse(&cli::command().get_matches());
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = match args.action {
        Action::List => runtime.block_on(list(&args.address)),
        Action::Attach {
            selector,
            wait_for_enter,
        } => attach(&runtime, &args.address, &selector, wait_for_enter),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logger() {
    let default_level = if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Warn
    };
    let _ = SimpleLogger::new()
        .with_utc_timestamps()
        .with_level(default_level)
        .env()
        .init();
}

async fn connect(address: &str) -> Result<TcpStream> {
    let stream = TcpStream::connect(address)
        .await
        .with_context(|| format!("connecting to {address}"))?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

async fn list(address: &str) -> Result<()> {
    let devices = client::list_devices(&mut connect(address).await?).await?;
    for d in devices {
        println!(
            "{}: {:04x}:{:04x} (bus {} dev {}, {:?} speed) {}",
            d.busid, d.id_vendor, d.id_product, d.busnum, d.devnum, d.speed, d.path
        );
        for (i, iface) in d.interfaces.iter().enumerate() {
            println!(
                "    {i}: class {:02x} subclass {:02x} protocol {:02x}",
                iface.class, iface.subclass, iface.protocol
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn import(address: &str, selector: &Selector) -> Result<client::Session> {
    let devices = client::list_devices(&mut connect(address).await?).await?;
    let device = selector
        .find(&devices)
        .ok_or_else(|| anyhow!("no exported device matches {selector:?}"))?;
    // The server closes the connection after a device list, so importing needs a new one.
    let session = client::import(connect(address).await?, &device.busid).await?;
    log::info!("imported {:?}", session.device());
    Ok(session)
}

#[cfg(not(target_os = "macos"))]
fn attach(
    _runtime: &tokio::runtime::Runtime,
    _address: &str,
    _selector: &Selector,
    _wait_for_enter: bool,
) -> Result<()> {
    Err(anyhow!("attaching devices is only supported on macOS"))
}

#[cfg(target_os = "macos")]
fn attach(
    runtime: &tokio::runtime::Runtime,
    address: &str,
    selector: &Selector,
    wait_for_enter: bool,
) -> Result<()> {
    use std::{io::Write, time::Duration};
    use tokio::signal::unix::{signal, SignalKind};
    use usbip_macos::host::Controller;

    let session = runtime.block_on(import(address, selector))?;
    let controller =
        Controller::new(session.clone(), runtime.handle().clone()).map_err(|e| anyhow!(e))?;

    runtime.block_on(controller.wait_powered());
    if wait_for_enter {
        print!("Press Enter to connect the device...");
        std::io::stdout().flush()?;
        std::io::stdin().read_line(&mut String::new())?;
    }
    controller.set_connected(true);

    let outcome = runtime.block_on(async {
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = term.recv() => Ok(()),
            reason = session.closed() => Err(anyhow!("connection lost: {reason}")),
        }
    });

    log::info!("detaching");
    controller.set_connected(false);
    session.unlink_all();
    runtime.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while session.pending() > 0 && !session.is_closed() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    });
    drop(controller);
    session.close();
    outcome
}
