// Round-trips reports through the macOS HID stack.
// usage: hid_probe <vid> <pid> <usage-page> <rounds>
// Each round sends a 64-byte output report and waits for the device to echo it as an input
// report. Prints "ok <rounds>" or exits non-zero.
import Foundation
import IOKit.hid

func hex(_ s: String) -> Int { Int(s.hasPrefix("0x") ? String(s.dropFirst(2)) : s, radix: 16)! }

let a = CommandLine.arguments
let (vid, pid, page, rounds) = (hex(a[1]), hex(a[2]), hex(a[3]), Int(a[4])!)

let manager = IOHIDManagerCreate(kCFAllocatorDefault, IOHIDOptionsType(kIOHIDOptionsTypeNone))
IOHIDManagerSetDeviceMatching(manager, [
    kIOHIDVendorIDKey: vid, kIOHIDProductIDKey: pid, kIOHIDPrimaryUsagePageKey: page,
] as CFDictionary)
IOHIDManagerScheduleWithRunLoop(manager, CFRunLoopGetCurrent(), CFRunLoopMode.defaultMode.rawValue)
guard IOHIDManagerOpen(manager, IOOptionBits(kIOHIDOptionsTypeNone)) == kIOReturnSuccess else {
    fatalError("cannot open HID manager")
}

var device: IOHIDDevice?
for _ in 0..<100 {
    if let set = IOHIDManagerCopyDevices(manager) as? Set<IOHIDDevice>, let d = set.first {
        device = d
        break
    }
    CFRunLoopRunInMode(.defaultMode, 0.1, false)
}
guard let device else { fatalError("no HID device \(a[1]):\(a[2]) page \(a[3])") }

final class Inbox { var reports: [[UInt8]] = [] }
let inbox = Inbox()
let buffer = UnsafeMutablePointer<UInt8>.allocate(capacity: 64)
IOHIDDeviceRegisterInputReportCallback(device, buffer, 64, { context, _, _, _, _, report, length in
    let inbox = Unmanaged<Inbox>.fromOpaque(context!).takeUnretainedValue()
    inbox.reports.append(Array(UnsafeBufferPointer(start: report, count: length)))
}, Unmanaged.passUnretained(inbox).toOpaque())

for round in 0..<rounds {
    let out = (0..<64).map { UInt8(truncatingIfNeeded: $0 * 3 + round) }
    let r = IOHIDDeviceSetReport(device, kIOHIDReportTypeOutput, 0, out, out.count)
    guard r == kIOReturnSuccess else { fatalError("set report failed: \(String(r, radix: 16))") }
    let deadline = Date().addingTimeInterval(5)
    while inbox.reports.isEmpty && Date() < deadline {
        CFRunLoopRunInMode(.defaultMode, 0.05, false)
    }
    guard let got = inbox.reports.first else { fatalError("round \(round): no input report") }
    inbox.reports.removeFirst()
    guard got == out else { fatalError("round \(round): echoed \(got) for \(out)") }
}
print("ok \(rounds)")
