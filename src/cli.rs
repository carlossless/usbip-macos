use clap::{arg, ArgGroup, ArgMatches, Command};
use clap_num::maybe_hex;

use crate::protocol::ExportedDevice;

pub fn command() -> Command {
    Command::new("usbip-macos")
        .about("USB/IP client for macOS")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Karolis Stasaitis")
        .arg_required_else_help(true)
        .subcommand_required(true)
        .arg(
            arg!(-r --remote <HOST> "The host to connect to")
                .value_parser(clap::value_parser!(String))
                .required(true),
        )
        .arg(
            arg!(-p --"tcp-port" <PORT> "The port to connect to")
                .value_parser(clap::value_parser!(u16))
                .default_value("3240"),
        )
        .subcommand(Command::new("list").about("List devices exported by the server"))
        .subcommand(
            Command::new("attach")
                .about("Attach an exported device")
                .arg(
                    arg!(--busid <BUSID> "Bus id of the device to attach")
                        .value_parser(clap::value_parser!(String)),
                )
                .arg(
                    arg!(--device <DEVNUM> "Device number on the server")
                        .value_parser(clap::value_parser!(u32)),
                )
                .arg(
                    arg!(--vendor_id <VENDOR_ID> "Vendor id of the device to attach")
                        .value_parser(maybe_hex::<u16>)
                        .requires("product_id"),
                )
                .arg(
                    arg!(--product_id <PRODUCT_ID> "Product id of the device to attach")
                        .value_parser(maybe_hex::<u16>)
                        .requires("vendor_id"),
                )
                .group(
                    ArgGroup::new("selector")
                        .args(["busid", "device", "vendor_id"])
                        .required(true),
                )
                .arg(
                    arg!(--"wait-for-enter" "Wait for Enter before connecting the port")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    BusId(String),
    DevNum(u32),
    Ids { vendor: u16, product: u16 },
}

impl Selector {
    pub fn matches(&self, device: &ExportedDevice) -> bool {
        match self {
            Self::BusId(busid) => device.busid == *busid,
            Self::DevNum(devnum) => device.devnum == *devnum,
            Self::Ids { vendor, product } => {
                device.id_vendor == *vendor && device.id_product == *product
            }
        }
    }

    pub fn find<'a>(&self, devices: &'a [ExportedDevice]) -> Option<&'a ExportedDevice> {
        devices.iter().find(|d| self.matches(d))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    List,
    Attach {
        selector: Selector,
        wait_for_enter: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub address: String,
    pub action: Action,
}

pub fn parse(matches: &ArgMatches) -> Args {
    let host = matches.get_one::<String>("remote").expect("required");
    let port = *matches.get_one::<u16>("tcp-port").expect("defaulted");
    let address = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let action = match matches.subcommand() {
        Some(("attach", m)) => {
            let selector = if let Some(busid) = m.get_one::<String>("busid") {
                Selector::BusId(busid.clone())
            } else if let Some(devnum) = m.get_one::<u32>("device") {
                Selector::DevNum(*devnum)
            } else {
                Selector::Ids {
                    vendor: *m.get_one::<u16>("vendor_id").expect("group"),
                    product: *m.get_one::<u16>("product_id").expect("requires"),
                }
            };
            Action::Attach {
                selector,
                wait_for_enter: m.get_flag("wait-for-enter"),
            }
        }
        _ => Action::List,
    };
    Args { address, action }
}

pub fn try_parse<I, T>(args: I) -> Result<Args, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    command().try_get_matches_from(args).map(|m| parse(&m))
}
