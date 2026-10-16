# usbip-macos

A [USB/IP](https://usbip.sourceforge.net/) client for macOS.

## Disclaimer

This is highly experimental and currently lacks various features for a full USB/IP implementation. It's has just enough working to work my current usecases. Use at your own risk.

## Permissions

Because this tool uses `IOUSBHostControllerInterface` it either needs to be signed with `com.apple.developer.usb.host-controller-interface` entitlement (which I don't have) or run as root on a system that has SIP disabled.

On macOS 27, SIP disabled alone was not enough; it also took the `amfi_get_out_of_my_way=1`
boot-arg (set from Recovery, with Reduced Security).

## What works and what cannot

| Transfer type | Status | Typical devices |
| --- | --- | --- |
| Control, bulk, interrupt | Supported | Storage, serial adapters, HID, network adapters, dev boards, cameras that stream over bulk |
| Isochronous OUT | Does not work for audio yet | Speakers, headphones |
| Isochronous IN | Cannot work | Most webcams, microphones, audio interfaces |
| Bulk streams (USB 3) | Not supported | UAS disks: the client hides UAS, with a warning, so they work as plain mass storage |

These limits come from macOS, not from USB/IP. Apple has said `IOUSBHostControllerInterface`
does not support isochronous endpoints or bulk streams, and other macOS clients built on it,
such as VirtualHere, have the same gaps.

**Isochronous transfers** are disabled in the kernel driver (`AppleUSBUserHCI`) unless the
undocumented boot-arg `AppleUSBUserHCI-isoch=1` is set. Without it, macOS refuses to open the
endpoint: an isochronous webcam or microphone shows up but never streams.

With the boot-arg set, isochronous IN still does not work: on macOS 27.0.1 the driver reports
each transfer as successful but never copies the received bytes back to the application, so
cameras deliver no frames and microphones record silence. The client cannot work around this.

Isochronous OUT reaches the device, but playback does not: macOS's audio daemon (`usbaudiod`)
queues about 160 ms of silence, the client delivers it, and then nothing more is queued while
the audio keeps "playing" on the Mac. A speaker with a microphone, such as the Studio
Display's, never gets that far: macOS restarts its streams continuously.

Hubs cannot be attached: Linux's `usbip-host` does not export them. Export the devices behind
a hub one at a time instead.

A webcam that streams video over a bulk endpoint, such as the Sony ZV-E10 in webcam mode, works.
`lsusb -v` on the server shows which kind a device uses: look for `Transfer Type Isochronous`
on its streaming interface.

A device that disconnects and comes back as a different device, such as one switching into a
DFU bootloader for a firmware update, ends the session. The client detaches and exits; bind
the new device on the server and attach it again.

macOS does not bring up a new network adapter while the screen is locked, so a USB Ethernet
adapter attached then only appears after unlocking and attaching it again.

USB/IP has no way to suspend the real device or to carry a remote wakeup back. macOS can still
suspend and resume an attached device; the device itself just stays awake, and it cannot wake
the Mac.

When macOS resets a device, for example to recover from an error or to start new firmware, the
client resets the real device on the server too.

Every transfer crosses the network, so a device that makes many small request/response
exchanges, such as a debug probe, runs at network speed: on a LAN with a 4 ms round trip, a
control transfer takes about 4 ms and a bulk write followed by a read about 7 ms. The client
itself adds about 0.2 ms.

### Tested with

- Low, full, high, SuperSpeed and SuperSpeedPlus devices (simulated), including multiple
  configurations, high-bandwidth interrupt endpoints and several devices attached at once.
- Mass storage and HID (Linux kernel gadgets), mounted and driven through macOS's own drivers.
- CDC ACM serial (an Embassy USB logger, a Nabu Casa ZBT-2) and a CP210x serial adapter (Nabu
  Casa SkyConnect), using macOS's built-in drivers.
- A Quectel EP06 LTE modem, driven from user space with AT commands.
- A Sony ZV-E10 as a webcam: 1280x720 at 25 fps.
- An EVGA XR1 Lite HDMI capture card: 1280x720 at about 18 fps over the LAN with the Mac on
  Wi-Fi (26 fps locally); its audio is isochronous and does not work.
- A SanDisk Extreme Portable SSD (UAS), used as plain mass storage: about 13 MB/s read over
  the LAN with the Mac on Wi-Fi.
- A SanDisk USB 3 stick (Bulk-Only Transport) over the LAN with the Mac on Wi-Fi: about 47 MB/s
  read and 21 MB/s write, against 160 and 43 MB/s locally.
- An Apple Studio Display: enumerates and its audio and HID functions bind, but its camera is
  isochronous and does not stream.
- Devices emulated on a [Cynthion](https://greatscottgadgets.com/cynthion/) with
  `scripts/emulate_device.py`: a bulk loopback, a FIDO-style HID, USB MIDI through CoreMIDI, an
  FTDI serial adapter through macOS's driver, mass storage, a CDC-ECM network adapter through
  macOS's network stack, and a device that re-enumerates.

## Testing

`cargo test` runs on macOS and Linux. The tests drive the client against a simulated USB/IP
server (`tests/support`) that parses the wire format independently and records protocol
violations.

The end-to-end tests in `tests/e2e_macos.rs` attach a simulated loopback device through the real
binary and talk to it from the macOS side with [nusb](https://github.com/kevinmehall/nusb). They
need the same permissions as the client itself, so they only run when asked to:

```sh
USBIP_E2E=1 cargo test --test e2e_macos -- --test-threads=1
```

`tests/e2e_real.rs` attaches devices exported by a real Linux `usbipd`, selected through
`USBIP_E2E_*` variables documented in the file. `scripts/linux-gadget.sh` can provide kernel
gadget devices for it; run that only in a disposable VM or on a spare board, because
`usbip-vudc` has hung the hosts it ran on.

`scripts/emulate_device.py` turns a Cynthion into one of several test devices, listed in the
script. Plug its CONTROL port into the machine running the script and its TARGET C port into the
usbipd host, then drive the device from macOS with the probes in `examples/` and
`tests/macos/`. Restart the Cynthion's gateware (`apollo reconfigure`) between runs; it degrades
when emulators are started and stopped repeatedly.

On the server, never run `usbip unbind` for a device that is disconnecting or already gone:
Linux 6.18's `usbip-host` dereferences a null pointer in that case and leaves the USB bus
deadlocked until a reboot.

To experiment by hand, serve the simulated device and attach it:

```sh
cargo run --example mock_server -- 127.0.0.1:3240
cargo run -- -r 127.0.0.1 attach --busid 1-1
```
