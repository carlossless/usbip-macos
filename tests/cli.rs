use usbip_macos::cli::{try_parse, Action, Args, Selector};
use usbip_macos::protocol::{ExportedDevice, Speed};

fn parse(args: &str) -> Result<Args, clap::Error> {
    try_parse(std::iter::once("usbip-macos").chain(args.split_whitespace()))
}

fn attach(args: &str) -> Selector {
    match parse(args).unwrap().action {
        Action::Attach { selector, .. } => selector,
        other => panic!("{other:?}"),
    }
}

#[test]
fn list() {
    let args = parse("-r host list").unwrap();
    assert_eq!(args.address, "host:3240");
    assert_eq!(args.action, Action::List);
    assert_eq!(parse("-r host -p 1234 list").unwrap().address, "host:1234");
    assert_eq!(parse("-r ::1 list").unwrap().address, "[::1]:3240");
}

#[test]
fn attach_selectors() {
    assert_eq!(
        attach("-r h attach --busid 1-1.2"),
        Selector::BusId("1-1.2".into())
    );
    assert_eq!(attach("-r h attach --device 4"), Selector::DevNum(4));
    assert_eq!(
        attach("-r h attach --vendor_id 0x05ac --product_id 591"),
        Selector::Ids {
            vendor: 0x05ac,
            product: 591
        }
    );
    match parse("-r h attach --busid 1-1 --wait-for-enter")
        .unwrap()
        .action
    {
        Action::Attach { wait_for_enter, .. } => assert!(wait_for_enter),
        other => panic!("{other:?}"),
    }
}

#[test]
fn invalid_invocations() {
    for args in [
        "list",
        "-r h",
        "-r h attach",
        "-r h attach --busid 1-1 --device 2",
        "-r h attach --busid 1-1 --vendor_id 1 --product_id 2",
        "-r h attach --vendor_id 1",
        "-r h attach --product_id 1",
        "-r h attach --vendor_id 0x10000 --product_id 1",
        "-r h -p 70000 list",
    ] {
        assert!(parse(args).is_err(), "accepted {args:?}");
    }
}

#[test]
fn selector_matching() {
    let device = |busid: &str, devnum, vid, pid| ExportedDevice {
        path: String::new(),
        busid: busid.into(),
        busnum: 1,
        devnum,
        speed: Speed::High,
        id_vendor: vid,
        id_product: pid,
        bcd_device: 0,
        device_class: 0,
        device_subclass: 0,
        device_protocol: 0,
        configuration_value: 1,
        num_configurations: 1,
        num_interfaces: 0,
        interfaces: vec![],
    };
    let devices = [
        device("1-1", 2, 1, 2),
        device("1-2", 3, 1, 3),
        device("1-3", 4, 1, 3),
    ];
    assert_eq!(
        Selector::BusId("1-2".into()).find(&devices).unwrap().devnum,
        3
    );
    assert_eq!(Selector::DevNum(4).find(&devices).unwrap().busid, "1-3");
    assert_eq!(
        Selector::Ids {
            vendor: 1,
            product: 3
        }
        .find(&devices)
        .unwrap()
        .busid,
        "1-2"
    );
    assert!(Selector::BusId("1-11".into()).find(&devices).is_none());
    assert!(Selector::Ids {
        vendor: 2,
        product: 3
    }
    .find(&devices)
    .is_none());
}
