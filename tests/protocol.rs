use bytes::BytesMut;
use proptest::prelude::*;
use usbip_macos::protocol::*;

fn be(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_be_bytes()).collect()
}

fn device_bytes(busid: &str, busnum: u32, devnum: u32, num_interfaces: u8) -> Vec<u8> {
    let mut out = vec![0u8; 256];
    out[..4].copy_from_slice(b"/sys");
    let mut b = [0u8; 32];
    b[..busid.len()].copy_from_slice(busid.as_bytes());
    out.extend_from_slice(&b);
    out.extend(be(&[busnum, devnum, 3]));
    out.extend_from_slice(&[
        0x12,
        0x34,
        0x56,
        0x78,
        0x01,
        0x00,
        9,
        0,
        1,
        1,
        1,
        num_interfaces,
    ]);
    out
}

#[test]
fn devlist_request_layout() {
    assert_eq!(
        encode_devlist_request(),
        [0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0]
    );
}

#[test]
fn import_request_layout() {
    let req = encode_import_request("1-1.2").unwrap();
    assert_eq!(&req[..8], &[0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0]);
    assert_eq!(&req[8..13], b"1-1.2");
    assert!(req[13..].iter().all(|&b| b == 0));
    assert_eq!(req.len(), 40);
}

#[test]
fn busid_validation() {
    assert!(encode_busid(&"x".repeat(31)).is_ok());
    assert!(
        encode_busid(&"x".repeat(32)).is_err(),
        "no room for the terminator"
    );
    assert!(encode_busid("a\0b").is_err());
}

#[tokio::test]
async fn devlist_reply_parses_devices_and_interfaces() {
    let mut reply = vec![0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0];
    reply.extend(be(&[2]));
    reply.extend(device_bytes("1-1", 1, 2, 2));
    reply.extend_from_slice(&[0x03, 0x01, 0x02, 0x00, 0xff, 0x42, 0x01, 0x00]);
    reply.extend(device_bytes("2-4", 2, 7, 0));
    let devices = read_devlist_reply(&mut &reply[..]).await.unwrap();
    assert_eq!(devices.len(), 2);
    let d = &devices[0];
    assert_eq!(d.busid, "1-1");
    assert_eq!(d.path, "/sys");
    assert_eq!((d.busnum, d.devnum, d.speed), (1, 2, Speed::High));
    assert_eq!(
        (d.id_vendor, d.id_product, d.bcd_device),
        (0x1234, 0x5678, 0x0100)
    );
    assert_eq!(d.devid(), 0x0001_0002);
    assert_eq!(
        d.interfaces,
        vec![
            Interface {
                class: 3,
                subclass: 1,
                protocol: 2
            },
            Interface {
                class: 0xff,
                subclass: 0x42,
                protocol: 1
            }
        ]
    );
    assert_eq!(devices[1].busid, "2-4");
    assert!(devices[1].interfaces.is_empty());
}

#[tokio::test]
async fn unterminated_strings_are_handled() {
    let mut reply = vec![0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0];
    reply.extend(be(&[1]));
    let mut dev = device_bytes("", 1, 1, 0);
    dev[..256].fill(b'p');
    dev[256..288].fill(b'b');
    reply.extend(dev);
    let devices = read_devlist_reply(&mut &reply[..]).await.unwrap();
    assert_eq!(devices[0].path.len(), 256);
    assert_eq!(devices[0].busid, "b".repeat(32));
}

#[tokio::test]
async fn devlist_reply_errors() {
    let mut wrong_code = vec![0x01, 0x11, 0x00, 0x03, 0, 0, 0, 0];
    wrong_code.extend(be(&[0]));
    assert!(matches!(
        read_devlist_reply(&mut &wrong_code[..]).await,
        Err(ProtocolError::UnexpectedReply {
            expected: 0x0005,
            actual: 0x0003
        })
    ));

    let failed = [0x01, 0x11, 0x00, 0x05, 0, 0, 0, 1];
    assert!(matches!(
        read_devlist_reply(&mut &failed[..]).await,
        Err(ProtocolError::Status(1))
    ));

    let mut huge = vec![0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0];
    huge.extend(be(&[u32::MAX]));
    assert!(matches!(
        read_devlist_reply(&mut &huge[..]).await,
        Err(ProtocolError::Invalid(_))
    ));

    let mut truncated = vec![0x01, 0x11, 0x00, 0x05, 0, 0, 0, 0];
    truncated.extend(be(&[1]));
    truncated.extend(&device_bytes("1-1", 1, 1, 1)[..100]);
    assert!(matches!(
        read_devlist_reply(&mut &truncated[..]).await,
        Err(ProtocolError::Io(_))
    ));
}

#[tokio::test]
async fn import_reply() {
    let mut reply = vec![0x01, 0x11, 0x00, 0x03, 0, 0, 0, 0];
    reply.extend(device_bytes("3-2", 3, 2, 4));
    let d = read_import_reply(&mut &reply[..]).await.unwrap();
    assert_eq!(d.busid, "3-2");
    assert_eq!(d.num_interfaces, 4);
    assert!(
        d.interfaces.is_empty(),
        "import replies carry no interface list"
    );

    let refused = [0x01, 0x11, 0x00, 0x03, 0, 0, 0, 1];
    assert!(matches!(
        read_import_reply(&mut &refused[..]).await,
        Err(ProtocolError::Status(1))
    ));
}

#[test]
fn cmd_submit_layout_out_with_data() {
    let cmd = CmdSubmit {
        seqnum: 7,
        devid: 0x0001_0002,
        direction: 0,
        ep: 2,
        transfer_flags: 0x40,
        transfer_buffer_length: 3,
        start_frame: 0,
        interval: 0,
        setup: [0; 8],
        data: vec![1, 2, 3],
        iso_packets: vec![],
    };
    let mut buf = BytesMut::new();
    cmd.encode(&mut buf);
    let mut expected = be(&[1, 7, 0x0001_0002, 0, 2, 0x40, 3, 0, 0, 0]);
    expected.extend([0u8; 8]);
    expected.extend([1, 2, 3]);
    assert_eq!(&buf[..], &expected[..]);
}

#[test]
fn cmd_submit_layout_control_and_iso() {
    let setup = [0x80, 6, 0, 1, 0, 0, 18, 0];
    let cmd = CmdSubmit {
        seqnum: 1,
        devid: 0x10002,
        direction: 1,
        ep: 0,
        transfer_buffer_length: 18,
        setup,
        ..Default::default()
    };
    let mut buf = BytesMut::new();
    cmd.encode(&mut buf);
    assert_eq!(buf.len(), 48, "IN transfers carry no data");
    assert_eq!(&buf[40..48], &setup);

    let iso = CmdSubmit {
        seqnum: 2,
        direction: 0,
        ep: 3,
        transfer_flags: URB_ISO_ASAP,
        transfer_buffer_length: 4,
        interval: 1,
        data: vec![9; 4],
        iso_packets: vec![IsoPacketDescriptor {
            offset: 0,
            length: 4,
            actual_length: 0,
            status: 0,
        }],
        ..Default::default()
    };
    let mut buf = BytesMut::new();
    iso.encode(&mut buf);
    assert_eq!(&buf[32..36], &1u32.to_be_bytes(), "number_of_packets");
    assert_eq!(&buf[48..52], &[9; 4]);
    assert_eq!(&buf[52..], &be(&[0, 4, 0, 0])[..]);
}

#[test]
fn cmd_unlink_layout() {
    let mut buf = BytesMut::new();
    CmdUnlink {
        seqnum: 9,
        devid: 0x20003,
        unlink_seqnum: 4,
    }
    .encode(&mut buf);
    let mut expected = be(&[2, 9, 0x20003, 0, 0, 4]);
    expected.resize(48, 0);
    assert_eq!(&buf[..], &expected[..]);
}

fn ret_submit(seqnum: u32, status: i32, actual: u32, packets: u32) -> Vec<u8> {
    let mut out = be(&[3, seqnum, 0, 0, 0, status as u32, actual, 5, packets, 1]);
    out.resize(48, 0);
    out
}

#[tokio::test]
async fn ret_submit_in_with_data() {
    let mut bytes = ret_submit(5, 0, 3, 0);
    bytes.extend([7, 8, 9]);
    let r = &mut &bytes[..];
    let header = read_reply_header(r).await.unwrap();
    let ret = read_submit_payload(r, &header, true, 64).await.unwrap();
    assert_eq!(ret.seqnum, 5);
    assert_eq!(ret.data, vec![7, 8, 9]);
    assert_eq!((ret.start_frame, ret.error_count), (5, 1));
    assert!(r.is_empty());
}

#[tokio::test]
async fn ret_submit_out_reports_length_without_data() {
    let bytes = ret_submit(5, 0, 512, 0);
    let r = &mut &bytes[..];
    let header = read_reply_header(r).await.unwrap();
    let ret = read_submit_payload(r, &header, false, 512).await.unwrap();
    assert_eq!(ret.actual_length, 512);
    assert!(ret.data.is_empty());
}

#[tokio::test]
async fn ret_submit_negative_status_and_no_packets_marker() {
    let bytes = ret_submit(1, -32, 0, 0xffff_ffff);
    let r = &mut &bytes[..];
    let header = read_reply_header(r).await.unwrap();
    let ret = read_submit_payload(r, &header, true, 8).await.unwrap();
    assert_eq!(ret.status, -32);
    assert!(ret.iso_packets.is_empty());
}

#[tokio::test]
async fn ret_submit_iso_descriptors() {
    let mut bytes = ret_submit(2, 0, 6, 2);
    bytes.extend([1, 2, 3, 4, 5, 6]);
    bytes.extend(be(&[0, 4, 4, 0, 4, 4, 2, (-18i32) as u32]));
    let r = &mut &bytes[..];
    let header = read_reply_header(r).await.unwrap();
    let ret = read_submit_payload(r, &header, true, 8).await.unwrap();
    assert_eq!(ret.iso_packets.len(), 2);
    assert_eq!(
        ret.iso_packets[1],
        IsoPacketDescriptor {
            offset: 4,
            length: 4,
            actual_length: 2,
            status: -18
        }
    );
    let unpacked = unpack_iso_in(&ret.data, &ret.iso_packets, 8).unwrap();
    assert_eq!(unpacked, vec![1, 2, 3, 4, 5, 6, 0, 0]);
}

#[test]
fn unpack_iso_rejects_inconsistent_descriptors() {
    let p = [IsoPacketDescriptor {
        offset: 6,
        length: 4,
        actual_length: 4,
        status: 0,
    }];
    assert!(
        unpack_iso_in(&[0; 4], &p, 8).is_none(),
        "packet beyond buffer"
    );
    let p = [IsoPacketDescriptor {
        offset: 0,
        length: 4,
        actual_length: 4,
        status: 0,
    }];
    assert!(
        unpack_iso_in(&[0; 2], &p, 8).is_none(),
        "missing packed data"
    );
}

#[tokio::test]
async fn ret_submit_longer_than_requested_is_rejected() {
    let mut bytes = ret_submit(1, 0, 100, 0);
    bytes.extend([0; 100]);
    let r = &mut &bytes[..];
    let header = read_reply_header(r).await.unwrap();
    assert!(matches!(
        read_submit_payload(r, &header, true, 64).await,
        Err(ProtocolError::Invalid(_))
    ));
}

#[tokio::test]
async fn too_many_iso_packets_is_rejected() {
    let bytes = ret_submit(1, 0, 0, 5000);
    assert!(matches!(
        read_reply_header(&mut &bytes[..]).await,
        Err(ProtocolError::Invalid(_))
    ));
}

#[tokio::test]
async fn ret_unlink_and_unknown_commands() {
    let mut bytes = be(&[4, 11, 0, 0, 0, (-104i32) as u32]);
    bytes.resize(48, 0);
    assert_eq!(
        read_reply_header(&mut &bytes[..]).await.unwrap(),
        Reply::Unlink(RetUnlink {
            seqnum: 11,
            status: -104
        })
    );
    let mut bytes = be(&[1, 1]);
    bytes.resize(48, 0);
    assert!(matches!(
        read_reply_header(&mut &bytes[..]).await,
        Err(ProtocolError::UnexpectedCommand(1))
    ));
}

#[test]
fn speed_and_direction() {
    assert_eq!(Speed::from_u32(5), Speed::Super);
    assert_eq!(Speed::from_u32(99), Speed::Unknown);
    assert_eq!(Direction::of_endpoint(0x81), Direction::In);
    assert_eq!(Direction::of_endpoint(0x01), Direction::Out);
}

proptest! {
    #[test]
    fn submit_frames_have_exact_length(
        data in proptest::collection::vec(any::<u8>(), 0..2048),
        packets in 0usize..8,
        dir_in in any::<bool>(),
    ) {
        let cmd = CmdSubmit {
            direction: dir_in as u32,
            transfer_buffer_length: data.len() as u32,
            data: if dir_in { vec![] } else { data.clone() },
            iso_packets: vec![IsoPacketDescriptor::default(); packets],
            ..Default::default()
        };
        let mut buf = BytesMut::new();
        cmd.encode(&mut buf);
        let payload = if dir_in { 0 } else { data.len() };
        prop_assert_eq!(buf.len(), 48 + payload + packets * 16);
    }

    #[test]
    fn ret_submit_roundtrip(
        seqnum in any::<u32>(),
        status in any::<i32>(),
        data in proptest::collection::vec(any::<u8>(), 0..1024),
        packets in proptest::collection::vec((any::<u32>(), any::<u32>(), any::<u32>(), any::<i32>()), 0..4),
    ) {
        let mut bytes = be(&[3, seqnum, 0, 0, 0, status as u32, data.len() as u32, 0, packets.len() as u32, 0]);
        bytes.resize(48, 0);
        bytes.extend(&data);
        for p in &packets {
            bytes.extend(be(&[p.0, p.1, p.2, p.3 as u32]));
        }
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let ret = rt.block_on(async {
            let r = &mut &bytes[..];
            let header = read_reply_header(r).await.unwrap();
            let ret = read_submit_payload(r, &header, true, data.len() as u32).await.unwrap();
            assert!(r.is_empty());
            ret
        });
        prop_assert_eq!(ret.seqnum, seqnum);
        prop_assert_eq!(ret.status, status);
        prop_assert_eq!(ret.data, data);
        prop_assert_eq!(ret.iso_packets.len(), packets.len());
    }

    #[test]
    fn arbitrary_reply_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let r = &mut &bytes[..];
            if let Ok(header) = read_reply_header(r).await {
                let _ = read_submit_payload(r, &header, true, 64).await;
            }
            let _ = read_devlist_reply(&mut &bytes[..]).await;
        });
    }
}

#[test]
fn endpoint_intervals_match_linux() {
    use usbip_macos::hci::{EndpointInfo, EndpointKind};
    let ep = |kind, b_interval| EndpointInfo {
        kind,
        max_packet_size: 64,
        b_interval,
    };
    assert_eq!(ep(EndpointKind::Interrupt, 4).urb_interval(Speed::High), 8);
    assert_eq!(
        ep(EndpointKind::Interrupt, 10).urb_interval(Speed::Full),
        10
    );
    assert_eq!(
        ep(EndpointKind::Interrupt, 0).urb_interval(Speed::Low),
        1,
        "never 0"
    );
    assert_eq!(
        ep(EndpointKind::Isochronous, 1).urb_interval(Speed::High),
        1
    );
    assert_eq!(
        ep(EndpointKind::Isochronous, 4).urb_interval(Speed::Full),
        8
    );
    assert_eq!(
        ep(EndpointKind::Isochronous, 3).urb_interval(Speed::Super),
        4
    );
    assert_eq!(
        ep(EndpointKind::Interrupt, 255).urb_interval(Speed::High),
        1 << 15
    );
    assert_eq!(ep(EndpointKind::Bulk, 5).urb_interval(Speed::High), 0);
    let parsed = EndpointInfo::parse(&[7, 5, 0x81, 3, 0x40, 0, 9]).unwrap();
    assert_eq!(parsed, ep(EndpointKind::Interrupt, 9));
    assert!(EndpointInfo::parse(&[7, 4, 0x81, 3, 0x40, 0, 9]).is_none());
}

#[test]
fn endpoint_descriptor_inside_configuration() {
    use usbip_macos::hci::{EndpointInfo, EndpointKind};
    // As passed by macOS for a non-default endpoint: a configuration header, then the endpoint.
    let wrapped = [
        9, 2, 22, 0, 0, 0, 0, 0, 0, 7, 5, 0x87, 3, 2, 0, 6, 6, 0x30, 0, 0, 0, 0,
    ];
    assert_eq!(
        EndpointInfo::parse(&wrapped),
        Some(EndpointInfo {
            kind: EndpointKind::Interrupt,
            max_packet_size: 2,
            b_interval: 6
        })
    );
    assert!(
        EndpointInfo::parse(&[9, 2, 9, 0, 0, 0, 0, 0, 0]).is_none(),
        "no endpoint inside"
    );
    assert!(
        EndpointInfo::parse(&[9, 2, 200, 0, 0, 0, 0, 0, 0, 0, 0]).is_none(),
        "zero-length descriptor"
    );
    assert!(
        EndpointInfo::parse(&[9, 2, 12, 0, 0, 0, 0, 0, 0, 7, 5, 0x81]).is_none(),
        "truncated"
    );
}
