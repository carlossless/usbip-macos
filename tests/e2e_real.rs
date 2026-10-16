//! Attaches devices exported by a real USB/IP server (Linux `usbipd`) and drives them from
//! macOS. Each test names the environment variable that selects its device; all of them need
//! `USBIP_E2E_REMOTE=host:port` as well. `scripts/linux-gadget.sh` exports kernel gadgets that
//! fit these tests without any hardware.
#![cfg(target_os = "macos")]

mod support;

use std::time::Duration;

use nusb::{
    descriptors::TransferType,
    transfer::{Buffer, Bulk, Direction, In, Out},
    MaybeFuture,
};
use support::macos::{enabled, sh, wait_for, wait_gone, Client, TIMEOUT};

struct Target {
    remote: String,
    busid: String,
    vid: u16,
    pid: u16,
}

/// Reads `busid:vid:pid` from `var`.
fn target(var: &str) -> Option<(Target, std::sync::MutexGuard<'static, ()>)> {
    let guard = enabled(var)?;
    let remote = std::env::var("USBIP_E2E_REMOTE").expect("USBIP_E2E_REMOTE=host:port");
    let spec = std::env::var(var).unwrap();
    let mut parts = spec.split(':');
    let busid = parts.next().unwrap().to_string();
    let hex = |p: Option<&str>| u16::from_str_radix(p.expect("busid:vid:pid"), 16).unwrap();
    let vid = hex(parts.next());
    let pid = hex(parts.next());
    Some((
        Target {
            remote,
            busid,
            vid,
            pid,
        },
        guard,
    ))
}

fn attach(t: &Target) -> (Client, nusb::DeviceInfo) {
    wait_gone(t.vid, t.pid);
    let mut client = Client::attach(&t.remote, &t.busid);
    let info = client.device(t.vid, t.pid);
    (client, info)
}

/// Enumeration, every descriptor and string, and a clean detach.
fn check_enumeration(t: &Target) -> String {
    let (mut client, info) = attach(t);
    let dev = info.open().wait().expect("open");
    let desc = dev.device_descriptor();
    for index in [
        desc.manufacturer_string_index(),
        desc.product_string_index(),
        desc.serial_number_string_index(),
    ]
    .into_iter()
    .flatten()
    {
        dev.get_string_descriptor(index, 0x0409, TIMEOUT)
            .wait()
            .expect("string descriptor");
    }
    let configs = dev.configurations().count();
    assert!(configs >= 1);
    let ioreg = sh(&format!(
        "ioreg -p IOUSB -l -w0 | grep -A40 '\"idProduct\" = {}' || true",
        t.pid
    ));
    drop(dev);
    assert!(client.terminate().success());
    wait_gone(t.vid, t.pid);
    ioreg
}

#[test]
fn any_device_enumerates() {
    let Some((t, _g)) = target("USBIP_E2E_DEVICE") else {
        return;
    };
    check_enumeration(&t);
}

#[test]
fn gadget_loopback() {
    let Some((t, _g)) = target("USBIP_E2E_GADGET") else {
        return;
    };
    let (mut client, info) = attach(&t);
    let dev = info.open().wait().unwrap();
    let config = dev.active_configuration().unwrap();
    let alt = config
        .interface_alt_settings()
        .find(|a| {
            a.class() == 0xff
                && a.endpoints()
                    .any(|e| e.transfer_type() == TransferType::Bulk)
        })
        .expect("vendor bulk interface");
    let ep_in = alt
        .endpoints()
        .find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == Direction::In)
        .unwrap();
    let ep_out = alt
        .endpoints()
        .find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == Direction::Out)
        .unwrap();
    let iface = dev.claim_interface(alt.interface_number()).wait().unwrap();
    let mut out = iface.endpoint::<Bulk, Out>(ep_out.address()).unwrap();
    let mut inp = iface.endpoint::<Bulk, In>(ep_in.address()).unwrap();
    let mps = ep_in.max_packet_size();
    for round in 0..200usize {
        let size = 1 + (round * 37) % 4096;
        // A whole number of packets would need a zero-length packet to end the transfer.
        let size = if size % 512 == 0 { size - 1 } else { size };
        let payload: Vec<u8> = (0..size).map(|i| (i + round) as u8).collect();
        let mut buf = Buffer::new(size);
        buf.extend_from_slice(&payload);
        out.transfer_blocking(buf, TIMEOUT).status.unwrap();
        let mut got = Vec::new();
        while got.len() < size {
            let c = inp.transfer_blocking(Buffer::new(4096usize.div_ceil(mps) * mps), TIMEOUT);
            c.status.unwrap();
            got.extend_from_slice(&c.buffer[..c.actual_len]);
        }
        assert_eq!(got, payload, "round {round} ({size} bytes)");
    }
    drop((out, inp, iface, dev));
    assert!(client.terminate().success());
}

#[test]
fn webcam_streams_frames() {
    let Some((t, _g)) = target("USBIP_E2E_WEBCAM") else {
        return;
    };
    let (_client, _info) = attach(&t);
    let name = std::env::var("USBIP_E2E_WEBCAM_NAME").unwrap_or_default();
    let fps = std::env::var("USBIP_E2E_WEBCAM_FPS").unwrap_or_else(|_| "30".into());
    wait_for("camera to register", Duration::from_secs(15), || {
        sh("system_profiler SPCameraDataType").contains(&name)
    });
    let out = std::env::temp_dir().join("usbip-webcam.jpg");
    let _ = std::fs::remove_file(&out);
    sh(&format!(
        "ffmpeg -hide_banner -loglevel error -f avfoundation -framerate {fps} -i '{name}' -frames:v 30 -update 1 -y {}",
        out.display()
    ));
    let size = std::fs::metadata(&out).expect("captured frame").len();
    assert!(size > 1000, "frame is only {size} bytes");
}

const GADGET_HELLO: &str = "hello from the linux gadget\n";

#[test]
fn gadget_mass_storage() {
    let Some((t, _g)) = target("USBIP_E2E_GADGET") else {
        return;
    };
    let (_client, _info) = attach(&t);
    let volume = std::path::Path::new("/Volumes/USBIPE2E");
    wait_for("volume to mount", Duration::from_secs(30), || {
        volume.join("HELLO.TXT").exists()
    });
    assert_eq!(
        std::fs::read_to_string(volume.join("HELLO.TXT")).unwrap(),
        GADGET_HELLO
    );

    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
    std::fs::write(volume.join("DATA.BIN"), &payload).unwrap();
    // Remounting drops the page cache, so the read below comes from the device.
    sh("sync; diskutil unmount /Volumes/USBIPE2E");
    let disk = sh("diskutil list | awk '/USBIPE2E/ {print $NF}'")
        .trim()
        .to_string();
    sh(&format!("diskutil mount {disk}"));
    wait_for("remount", Duration::from_secs(20), || {
        volume.join("DATA.BIN").exists()
    });
    assert!(
        std::fs::read(volume.join("DATA.BIN")).unwrap() == payload,
        "read back differs"
    );
    std::fs::remove_file(volume.join("DATA.BIN")).unwrap();
    sh("sync; diskutil unmount /Volumes/USBIPE2E");
}

#[test]
fn gadget_serial_echo() {
    use std::io::{Read, Write};
    let Some((t, _g)) = target("USBIP_E2E_GADGET") else {
        return;
    };
    let (_client, _info) = attach(&t);
    let tty = || {
        std::fs::read_dir("/dev")
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.to_string_lossy().starts_with("/dev/cu.usbmodemusbip_e2e"))
    };
    wait_for("serial port", Duration::from_secs(20), || tty().is_some());
    let tty = tty().unwrap();
    sh(&format!("stty -f {} raw -echo 115200", tty.display()));
    let mut port = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tty)
        .unwrap();
    let mut reader = port.try_clone().unwrap();
    let payload: Vec<u8> = (0..16_384u32).map(|i| (i % 251) as u8).collect();
    let expected = payload.len();
    let echoed = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while got.len() < expected {
            let n = reader.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        got
    });
    for chunk in payload.chunks(1000) {
        port.write_all(chunk).unwrap();
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !echoed.is_finished() {
        assert!(std::time::Instant::now() < deadline, "echo timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(echoed.join().unwrap() == payload, "echo differs");
}

#[test]
fn gadget_hid_reports() {
    let Some((t, _g)) = target("USBIP_E2E_GADGET") else {
        return;
    };
    let (_client, _info) = attach(&t);
    let probe = support::macos::swift_probe("hid_probe");
    let (ok, out) = support::macos::run_for(
        Duration::from_secs(60),
        std::process::Command::new(probe).args([
            &format!("{:04x}", t.vid),
            &format!("{:04x}", t.pid),
            "ff00",
            "50",
        ]),
    );
    assert!(ok && out.contains("ok 50"), "{out}");
}

#[test]
fn gadget_audio_playback() {
    let Some((t, _g)) = target("USBIP_E2E_GADGET") else {
        return;
    };
    let (_client, _info) = attach(&t);
    // CoreAudio names f_uac2 devices after their streaming interface string.
    let name = "Playback Inactive";
    wait_for("audio device", Duration::from_secs(20), || {
        sh("system_profiler SPAudioDataType").contains(name)
    });
    // `say` streams to the device through CoreAudio and fails if the device rejects the stream.
    // This cannot check the samples: usbip-vudc drops isochronous OUT data even from Linux's own
    // client, so the data path is covered by the engine tests against the simulated server.
    let (ok, out) = support::macos::run_for(
        Duration::from_secs(60),
        std::process::Command::new("say").args(["-a", name, "testing usb audio over usbip"]),
    );
    assert!(ok, "say failed: {out}");
}

/// Sends `ATI` to every vendor-specific interface with a bulk pair until one answers. macOS has
/// no driver for these modem ports, so this talks to them directly.
#[test]
fn modem_answers_at_commands() {
    let Some((t, _g)) = target("USBIP_E2E_MODEM") else {
        return;
    };
    let (_client, info) = attach(&t);
    let dev = info.open().wait().unwrap();
    let config = dev.active_configuration().unwrap();
    let mut answers = Vec::new();
    // Subclass 0 skips the diagnostic and QMI interfaces, which should not see stray AT text.
    for alt in config
        .interface_alt_settings()
        .filter(|a| a.class() == 0xff && a.subclass() == 0 && a.alternate_setting() == 0)
    {
        let bulk = |d| {
            alt.endpoints()
                .find(|e| e.transfer_type() == TransferType::Bulk && e.direction() == d)
        };
        let (Some(ep_in), Some(ep_out)) = (bulk(Direction::In), bulk(Direction::Out)) else {
            continue;
        };
        let Ok(iface) = dev.claim_interface(alt.interface_number()).wait() else {
            continue;
        };
        let mut out = iface.endpoint::<Bulk, Out>(ep_out.address()).unwrap();
        let mut inp = iface.endpoint::<Bulk, In>(ep_in.address()).unwrap();
        let mut cmd = Buffer::new(4);
        cmd.extend_from_slice(b"ATI\r");
        if out.transfer_blocking(cmd, TIMEOUT).status.is_err() {
            continue;
        }
        let mut reply = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && !String::from_utf8_lossy(&reply).contains("OK")
        {
            let c = inp.transfer_blocking(
                Buffer::new(ep_in.max_packet_size()),
                Duration::from_millis(500),
            );
            if c.status.is_ok() {
                reply.extend_from_slice(&c.buffer[..c.actual_len]);
            }
        }
        answers.push((
            alt.interface_number(),
            String::from_utf8_lossy(&reply).into_owned(),
        ));
    }
    let at = answers.iter().find(|(_, r)| r.contains("OK"));
    assert!(at.is_some(), "no interface answered ATI: {answers:?}");
    eprintln!(
        "AT port on interface {}: {:?}",
        at.unwrap().0,
        at.unwrap().1
    );
}
