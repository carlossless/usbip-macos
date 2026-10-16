#!/usr/bin/env python3
"""Emulates USB devices on a Cynthion (Facedancer gateware) for the real-device tests.

usage: emulate_device.py <device> [--full-speed] [--verbose] [--trace]

Devices (vendor 0x1209 unless noted):
  loopback     0x7360  vendor interface, bulk OUT 0x01 echoed on bulk IN 0x81
  fido         0x7361  HID on the FIDO usage page, 64-byte output reports echoed as input reports
  midi         0x7362  USB MIDI 1.0, every event received is echoed back
  reenumerate  0x7363  vendor request 0x01 makes it disconnect and return as 0x7364, like a
                       DFU bootloader
  serial       0403:6001  FTDI serial adapter that echoes what it receives
  storage      0x7365  64 MiB mass storage backed by facedancer-disk.img (created if missing)
  ethernet     0x7366  CDC-ECM network adapter; a responder behind it at 10.98.0.1 answers
                       ARP and ping. Frames that are a whole number of 512-byte packets are
                       not supported.

Plug the Cynthion's CONTROL port into the machine running this script and its TARGET C port
into the machine that runs usbipd.
"""

import asyncio
import collections
import os
import sys
import time

from facedancer import *
from facedancer.devices.ftdi import FTDIDevice
from facedancer.devices.umass.umass import USBMassStorageDevice
from facedancer.devices.umass.disk_image import RawDiskImage
from facedancer.logging import configure_default_logging

VID = 0x1209


@standard_request_handler(number=USBStandardRequests.SET_FEATURE)
@to_device
def acknowledge_set_feature(self, request):
    request.acknowledge()


# macOS sets DEVICE_REMOTE_WAKEUP after configuring; Facedancer stalls it by default, and over
# usbip the stalls left the Cynthion's control endpoint unresponsive.
USBDevice.handle_set_feature_request = acknowledge_set_feature


class EchoQueue:
    """Bytes received on OUT, handed back on IN one packet at a time.

    A NAK on IN means the endpoint's buffer is empty. Passing the IN endpoint to `received`
    sends straight away when it is, since a host that keeps an IN pending for long floods the
    Cynthion with NAKs and it stops accepting OUT data.
    """

    def __init__(self):
        self.packets = collections.deque()
        self.primed = False

    def received(self, data, packet_size, endpoint_in=None):
        for i in range(0, len(data), packet_size):
            self.packets.append(data[i:i + packet_size])
        if endpoint_in is not None and not self.primed:
            self.send(endpoint_in)

    def requested(self, endpoint):
        self.primed = False
        self.send(endpoint)

    def send(self, endpoint):
        if self.packets:
            endpoint.send(self.packets.popleft())
            self.primed = True


@use_inner_classes_automatically
class Loopback(USBDevice):
    vendor_id: int = VID
    product_id: int = 0x7360
    manufacturer_string: str = "usbip-macos tests"
    product_string: str = "Facedancer loopback"
    serial_number_string: str = "FDLOOP1"
    device_speed: DeviceSpeed = DeviceSpeed.HIGH

    class _Configuration(USBConfiguration):
        class _Interface(USBInterface):
            class_number: int = 0xFF

            class _Out(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.OUT
                transfer_type: USBTransferType = USBTransferType.BULK
                max_packet_size: int = 512

            class _In(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.IN
                transfer_type: USBTransferType = USBTransferType.BULK
                max_packet_size: int = 512

    def __post_init__(self):
        super().__post_init__()
        self.echo = EchoQueue()

    def handle_data_received(self, endpoint, data):
        self.echo.received(data, endpoint.max_packet_size)

    def handle_data_requested(self, endpoint):
        self.echo.requested(endpoint)


FIDO_REPORT = bytes([
    0x06, 0xD0, 0xF1,        # usage page (FIDO)
    0x09, 0x01,              # usage (CTAPHID)
    0xA1, 0x01,              # collection (application)
    0x09, 0x20, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x40, 0x81, 0x02,
    0x09, 0x21, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x40, 0x91, 0x02,
    0xC0,
])


@use_inner_classes_automatically
class Fido(USBDevice):
    vendor_id: int = VID
    product_id: int = 0x7361
    manufacturer_string: str = "usbip-macos tests"
    product_string: str = "Facedancer FIDO"
    serial_number_string: str = "FDFIDO1"
    device_speed: DeviceSpeed = DeviceSpeed.FULL

    class _Configuration(USBConfiguration):
        class _Interface(USBInterface):
            class_number: int = 3

            class _Hid(USBDescriptor):
                number: int = 0
                type_number: int = 0x21
                raw: bytes = bytes([9, 0x21, 0x11, 0x01, 0, 1, 0x22, len(FIDO_REPORT), 0])
                include_in_config: bool = True

            class _Report(USBDescriptor):
                number: int = 0
                type_number: int = 0x22
                raw: bytes = FIDO_REPORT
                include_in_config: bool = False

            class _In(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.IN
                transfer_type: USBTransferType = USBTransferType.INTERRUPT
                max_packet_size: int = 64
                interval: int = 5

            class _Out(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.OUT
                transfer_type: USBTransferType = USBTransferType.INTERRUPT
                max_packet_size: int = 64
                interval: int = 5

            @class_request_handler(number=0x0A)
            @to_this_interface
            def handle_set_idle(self, request):
                request.ack()

    def __post_init__(self):
        super().__post_init__()
        self.echo = EchoQueue()

    def handle_data_received(self, endpoint, data):
        self.echo.received(data, 64, self.get_endpoint(1, USBDirection.IN))

    def handle_data_requested(self, endpoint):
        self.echo.requested(endpoint)


def midi_configuration():
    jacks = bytes([
        6, 0x24, 0x02, 0x01, 0x01, 0,                    # embedded IN jack 1
        6, 0x24, 0x02, 0x02, 0x02, 0,                    # external IN jack 2
        9, 0x24, 0x03, 0x01, 0x03, 1, 0x02, 0x01, 0,     # embedded OUT jack 3 <- 2
        9, 0x24, 0x03, 0x02, 0x04, 1, 0x01, 0x01, 0,     # external OUT jack 4 <- 1
    ])
    endpoints = bytes([
        9, 0x05, 0x01, 0x02, 0x40, 0x00, 0, 0, 0,
        5, 0x25, 0x01, 1, 0x01,
        9, 0x05, 0x81, 0x02, 0x40, 0x00, 0, 0, 0,
        5, 0x25, 0x01, 1, 0x03,
    ])
    ms_total = 7 + len(jacks) + len(endpoints)
    body = bytes([
        9, 0x04, 0, 0, 0, 0x01, 0x01, 0x00, 0,
        9, 0x24, 0x01, 0x00, 0x01, 9, 0, 1, 1,
        9, 0x04, 1, 0, 2, 0x01, 0x03, 0x00, 0,
        7, 0x24, 0x01, 0x00, 0x01, ms_total & 0xFF, ms_total >> 8,
    ]) + jacks + endpoints
    total = 9 + len(body)
    return bytes([9, 0x02, total & 0xFF, total >> 8, 2, 1, 0, 0x80, 50]) + body


@use_inner_classes_automatically
class Midi(USBDevice):
    vendor_id: int = VID
    product_id: int = 0x7362
    manufacturer_string: str = "usbip-macos tests"
    product_string: str = "Facedancer MIDI"
    serial_number_string: str = "FDMIDI1"
    device_speed: DeviceSpeed = DeviceSpeed.FULL

    # Routing only: the descriptors come from midi_configuration(), which Facedancer cannot
    # express (class-specific jacks and 9-byte audio endpoint descriptors).
    class _Configuration(USBConfiguration):
        class _Control(USBInterface):
            number: int = 0
            class_number: int = 1
            subclass_number: int = 1

        class _Streaming(USBInterface):
            number: int = 1
            class_number: int = 1
            subclass_number: int = 3

            class _Out(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.OUT
                transfer_type: USBTransferType = USBTransferType.BULK
                max_packet_size: int = 64

            class _In(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.IN
                transfer_type: USBTransferType = USBTransferType.BULK
                max_packet_size: int = 64

    def __post_init__(self):
        super().__post_init__()
        self.echo = EchoQueue()

    def get_configuration_descriptor(self, index):
        return midi_configuration()

    def handle_data_received(self, endpoint, data):
        self.echo.received(data, 64, self.get_endpoint(1, USBDirection.IN))

    def handle_data_requested(self, endpoint):
        self.echo.requested(endpoint)


@use_inner_classes_automatically
class Reenumerate(USBDevice):
    vendor_id: int = VID
    product_id: int = 0x7363
    manufacturer_string: str = "usbip-macos tests"
    product_string: str = "Facedancer re-enumeration"
    serial_number_string: str = "FDREENUM1"
    device_speed: DeviceSpeed = DeviceSpeed.HIGH

    class _Configuration(USBConfiguration):
        class _Interface(USBInterface):
            class_number: int = 0xFF

    def __post_init__(self):
        super().__post_init__()
        self.detach = asyncio.Event()

    @vendor_request_handler(number=0x01, direction=USBDirection.OUT)
    @to_device
    def handle_detach(self, request):
        request.ack()
        self.detach.set()

    async def bootloader(self):
        await self.detach.wait()
        await asyncio.sleep(0.2)
        self.disconnect()
        await asyncio.sleep(1)
        self.product_id = 0x7364
        self.product_string = "Facedancer re-enumeration (bootloader)"
        self.connect()


PEER_MAC = bytes.fromhex("020000000001")
PEER_IP = bytes([10, 98, 0, 1])


def checksum(data):
    if len(data) % 2:
        data += b"\0"
    total = sum(int.from_bytes(data[i:i + 2], "big") for i in range(0, len(data), 2))
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total & 0xFFFF).to_bytes(2, "big")


def answer(frame):
    """The reply of a host at PEER_IP to an Ethernet frame, if it warrants one."""
    ethertype = frame[12:14]
    if ethertype == b"\x08\x06" and frame[20:22] == b"\x00\x01" and frame[38:42] == PEER_IP:
        sender_mac, sender_ip = frame[22:28], frame[28:32]
        return (sender_mac + PEER_MAC + b"\x08\x06" + b"\x00\x01\x08\x00\x06\x04\x00\x02"
                + PEER_MAC + PEER_IP + sender_mac + sender_ip)
    if ethertype != b"\x08\x00" or frame[30:34] != PEER_IP or frame[23] != 1:
        return None
    header_length = (frame[14] & 0x0F) * 4
    ip = bytearray(frame[14:14 + header_length])
    icmp = bytearray(frame[14 + header_length:14 + int.from_bytes(frame[16:18], "big")])
    if not icmp or icmp[0] != 8:
        return None
    ip[12:16], ip[16:20] = frame[30:34], frame[26:30]
    ip[8] = 64
    ip[10:12] = b"\0\0"
    ip[10:12] = checksum(bytes(ip))
    icmp[0] = 0
    icmp[2:4] = b"\0\0"
    icmp[2:4] = checksum(bytes(icmp))
    return frame[6:12] + PEER_MAC + b"\x08\x00" + bytes(ip) + bytes(icmp)


def ecm_configuration():
    communication = bytes([
        9, 0x04, 0, 0, 1, 0x02, 0x06, 0x00, 0,
        5, 0x24, 0x00, 0x10, 0x01,                                  # header
        5, 0x24, 0x06, 0, 1,                                        # union
        13, 0x24, 0x0F, 4, 0, 0, 0, 0, 0xEA, 0x05, 0, 0, 0,         # Ethernet, MAC string 4
        7, 0x05, 0x83, 0x03, 16, 0, 8,
    ])
    data = bytes([
        9, 0x04, 1, 0, 0, 0x0A, 0, 0, 0,
        9, 0x04, 1, 1, 2, 0x0A, 0, 0, 0,
        7, 0x05, 0x01, 0x02, 0x00, 0x02, 0,
        7, 0x05, 0x81, 0x02, 0x00, 0x02, 0,
    ])
    total = 9 + len(communication) + len(data)
    return bytes([9, 0x02, total & 0xFF, total >> 8, 2, 1, 0, 0x80, 50]) + communication + data


@use_inner_classes_automatically
class Ethernet(USBDevice):
    vendor_id: int = VID
    product_id: int = 0x7366
    manufacturer_string: str = "usbip-macos tests"
    product_string: str = "Facedancer Ethernet"
    serial_number_string: str = "FDETH1"
    device_speed: DeviceSpeed = DeviceSpeed.HIGH

    # Routing only: the descriptors come from ecm_configuration().
    class _Configuration(USBConfiguration):
        class _Communication(USBInterface):
            number: int = 0
            class_number: int = 0x02
            subclass_number: int = 0x06

            class _Notify(USBEndpoint):
                number: int = 3
                direction: USBDirection = USBDirection.IN
                transfer_type: USBTransferType = USBTransferType.INTERRUPT
                max_packet_size: int = 16
                interval: int = 8

            @class_request_handler(number=0x44)
            @to_this_interface
            def handle_get_statistic(self, request):
                request.reply(bytes(4))

            @class_request_handler(condition=lambda r: 0x40 <= r.number <= 0x43)
            @to_this_interface
            def handle_filters(self, request):
                request.ack()

        class _Idle(USBInterface):
            number: int = 1
            alternate: int = 0
            class_number: int = 0x0A

        class _Data(USBInterface):
            number: int = 1
            alternate: int = 1
            class_number: int = 0x0A

            class _Out(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.OUT
                transfer_type: USBTransferType = USBTransferType.BULK
                max_packet_size: int = 512

            class _In(USBEndpoint):
                number: int = 1
                direction: USBDirection = USBDirection.IN
                transfer_type: USBTransferType = USBTransferType.BULK
                max_packet_size: int = 512

    def __post_init__(self):
        super().__post_init__()
        self.received = bytearray()
        self.frames = EchoQueue()
        self.notifications = EchoQueue()
        self.announced = 0.0

    def get_configuration_descriptor(self, index):
        return ecm_configuration()

    def get_string_descriptor(self, index):
        if index == 4:
            text = "020000C0FFEE".encode("utf-16-le")
            return bytes([2 + len(text), 3]) + text
        return super().get_string_descriptor(index)

    def handle_data_received(self, endpoint, data):
        self.received += data
        if len(data) == endpoint.max_packet_size:
            return
        frame, self.received = bytes(self.received), bytearray()
        reply = answer(frame)
        if reply is not None:
            self.frames.received(reply, 512, self.get_endpoint(1, USBDirection.IN))

    def handle_data_requested(self, endpoint):
        if endpoint.number != 3:
            self.frames.requested(endpoint)
            return
        # Repeated, because whichever host polls first takes them: Linux's own driver, briefly,
        # before the device is exported.
        if not self.notifications.packets and time.monotonic() - self.announced > 1:
            self.announced = time.monotonic()
            speed = (480_000_000).to_bytes(4, "little") * 2
            self.notifications.received(bytes([0xA1, 0x00, 1, 0, 0, 0, 0, 0]), 16)
            self.notifications.received(bytes([0xA1, 0x2A, 0, 0, 0, 0, 8, 0]) + speed, 16)
        self.notifications.requested(endpoint)


def serial():
    # Patched on the instance: a subclass would lose FTDIDevice's inner configuration classes.
    device = FTDIDevice()
    device.handle_serial_data_received = lambda data: device.transmit(data, adjust_endings=False)
    return device


def storage():
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "facedancer-disk.img")
    if not os.path.exists(path):
        with open(path, "wb") as f:
            f.truncate(64 * 1024 * 1024)
    device = USBMassStorageDevice(RawDiskImage(path, 512))
    device.vendor_id = VID
    device.product_id = 0x7365
    return device


def trace(device):
    """Prints control requests and data events; cheap enough not to stall the emulation."""
    handle_request = device.handle_request
    handle_data_received = device.handle_data_received
    handle_data_requested = device.handle_data_requested

    def request(req):
        print(f"ctrl {req.request_type:02x} {req.request:02x} value={req.value:04x} "
              f"index={req.index:04x} length={req.length}", flush=True)
        handle_request(req)

    def received(endpoint, data):
        print(f"OUT ep{endpoint.number} {len(data)} bytes", flush=True)
        handle_data_received(endpoint, data)

    def requested(endpoint):
        print(f"IN ep{endpoint.number} requested", flush=True)
        handle_data_requested(endpoint)

    device.handle_request = request
    device.handle_data_received = received
    device.handle_data_requested = requested


def main():
    configure_default_logging(level=10 if "--verbose" in sys.argv else 30)
    which = sys.argv[1] if len(sys.argv) > 1 else ""
    devices = {
        "loopback": Loopback,
        "fido": Fido,
        "midi": Midi,
        "reenumerate": Reenumerate,
        "serial": serial,
        "storage": storage,
        "ethernet": Ethernet,
    }
    if which not in devices:
        sys.exit(__doc__)
    device = devices[which]()
    if "--full-speed" in sys.argv:
        device.device_speed = DeviceSpeed.FULL
    if "--trace" in sys.argv:
        trace(device)
    coroutines = [device.bootloader()] if isinstance(device, Reenumerate) else []
    print(f"emulating {which} {device.vendor_id:04x}:{device.product_id:04x}", flush=True)
    device.emulate(*coroutines)


if __name__ == "__main__":
    main()
