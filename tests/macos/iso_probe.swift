// Runs isochronous transfers through IOUSBHost, which nusb cannot do.
// usage: iso_probe <vid> <pid> <interface> <alt> <endpoint> <frames> <bytes-per-frame> [out]
// Prints one line per frame: "<status> <completed bytes> <hex of first 8 bytes>".
import Foundation
import IOKit
import IOUSBHost

func hex(_ s: String) -> Int { Int(s.hasPrefix("0x") ? String(s.dropFirst(2)) : s, radix: 16)! }

let a = CommandLine.arguments
let (vid, pid) = (hex(a[1]), hex(a[2]))
let (ifaceNumber, alt, address) = (Int(a[3])!, Int(a[4])!, hex(a[5]))
let (count, length) = (Int(a[6])!, Int(a[7])!)
let isOut = a.count > 8 && a[8] == "out"

func service(_ matching: CFMutableDictionary) -> io_service_t {
    for _ in 0..<100 {
        let s = IOServiceGetMatchingService(kIOMainPortDefault, CFDictionaryCreateMutableCopy(nil, 0, matching))
        if s != 0 { return s }
        usleep(100_000)
    }
    fatalError("no matching service")
}

let device = try IOUSBHostDevice(
    __ioService: service(IOUSBHostDevice.__createMatchingDictionary(
        withVendorID: NSNumber(value: vid), productID: NSNumber(value: pid), bcdDevice: nil,
        deviceClass: nil, deviceSubclass: nil, deviceProtocol: nil, speed: nil, productIDArray: nil).takeRetainedValue()),
    options: [], queue: nil, interestHandler: nil)
FileHandle.standardError.write("device found\n".data(using: .utf8)!)
try device.__configure(withValue: 1, matchInterfaces: true)
FileHandle.standardError.write("configured\n".data(using: .utf8)!)

func child(of parent: io_service_t, interface number: Int) -> io_service_t {
    for _ in 0..<100 {
        var it: io_iterator_t = 0
        IORegistryEntryGetChildIterator(parent, kIOServicePlane, &it)
        while case let c = IOIteratorNext(it), c != 0 {
            let n = IORegistryEntryCreateCFProperty(c, "bInterfaceNumber" as CFString, nil, 0)?.takeRetainedValue() as? Int
            if n == number { return c }
            IOObjectRelease(c)
        }
        IOObjectRelease(it)
        usleep(100_000)
    }
    fatalError("interface \(number) not found")
}

let iface = try IOUSBHostInterface(
    __ioService: child(of: device.ioService, interface: ifaceNumber),
    options: [], queue: nil, interestHandler: nil)
try iface.selectAlternateSetting(alt)
let pipe = try iface.copyPipe(withAddress: address)

let data: NSMutableData = ProcessInfo.processInfo.environment["ISO_PLAIN"] != nil
    ? NSMutableData(length: count * length)!
    : try iface.ioData(withCapacity: count * length)
if isOut {
    let bytes = data.mutableBytes.assumingMemoryBound(to: UInt8.self)
    for i in 0..<(count * length) { bytes[i] = UInt8(truncatingIfNeeded: i * 7) }
}
var frames = (0..<count).map {
    IOUSBHostIsochronousTransaction(status: 0, requestCount: UInt32(length), offset: UInt32($0 * length), completeCount: 0, timeStamp: 0, options: [])
}
let lead = UInt64(ProcessInfo.processInfo.environment["ISO_LEAD"] ?? "20")!
let now = iface.__frameNumber(withTime: nil)
let first = now + lead
FileHandle.standardError.write("frame \(now) first \(first)\n".data(using: .utf8)!)
try frames.withUnsafeMutableBufferPointer { buf in
    try pipe.sendIORequest(with: data, transactionList: buf.baseAddress!, transactionListCount: count, firstFrameNumber: first, options: [])
}
let bytes = data.bytes.assumingMemoryBound(to: UInt8.self)
for (i, f) in frames.enumerated() {
    let head = (0..<min(8, Int(f.completeCount))).map { String(format: "%02x", bytes[i * length + $0]) }.joined()
    print(f.status, f.completeCount, head)
}
