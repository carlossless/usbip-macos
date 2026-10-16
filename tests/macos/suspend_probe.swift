// Lets macOS suspend an idle device, then wakes it with I/O.
// usage: suspend_probe <vid> <pid> <idle-seconds>
// Opens interface 0, sets a short idle timeout so macOS suspends the device, waits, then sends
// a bulk OUT on 0x01 and reads it back on 0x81 (a loopback). Prints "ok" or exits non-zero.
import Foundation
import IOUSBHost

func hex(_ s: String) -> Int { Int(s.hasPrefix("0x") ? String(s.dropFirst(2)) : s, radix: 16)! }

let a = CommandLine.arguments
let (vid, pid, idle) = (hex(a[1]), hex(a[2]), Double(a[3])!)

let matching: [String: Any] = [
    "IOProviderClass": "IOUSBHostInterface",
    "IOPropertyMatch": ["idVendor": vid, "idProduct": pid, "bInterfaceNumber": 0],
]
let service = IOServiceGetMatchingService(kIOMainPortDefault, matching as CFDictionary)
guard service != 0 else { fatalError("no interface for \(a[1]):\(a[2])") }

do {
    let iface = try IOUSBHostInterface(__ioService: service, options: [], queue: nil, interestHandler: nil)
    try iface.setIdleTimeout(0.1)
    let out = try iface.copyPipe(withAddress: 0x01)
    let inp = try iface.copyPipe(withAddress: 0x81)
    try out.setIdleTimeout(0.1)
    try inp.setIdleTimeout(0.1)
    Thread.sleep(forTimeInterval: idle)

    let message = Data("after suspend".utf8)
    var sent = 0
    try out.__sendIORequest(with: NSMutableData(data: message), bytesTransferred: &sent, completionTimeout: 5)
    let buffer = NSMutableData(length: 512)!
    var received = 0
    try inp.__sendIORequest(with: buffer, bytesTransferred: &received, completionTimeout: 5)
    let got = buffer.subdata(with: NSRange(location: 0, length: received))
    guard got == message else { fatalError("sent \(message), got \(got as NSData)") }
    print("ok")
} catch {
    fatalError("\(error)")
}
