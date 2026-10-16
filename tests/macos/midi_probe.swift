// Round-trips MIDI messages through CoreMIDI.
// usage: midi_probe <device-name> <rounds>
// Each round sends a note-on to the device's destination and waits for the device to echo it
// on its source. Prints "ok <rounds>" or exits non-zero.
import CoreMIDI
import Foundation

let a = CommandLine.arguments
let (name, rounds) = (a[1], Int(a[2])!)

func endpointName(_ e: MIDIEndpointRef) -> String {
    var cf: Unmanaged<CFString>?
    MIDIObjectGetStringProperty(e, kMIDIPropertyDisplayName, &cf)
    return (cf?.takeRetainedValue() as String?) ?? ""
}

func find(_ count: () -> Int, _ get: (Int) -> MIDIEndpointRef) -> MIDIEndpointRef? {
    for _ in 0..<100 {
        for i in 0..<count() where endpointName(get(i)).contains(name) {
            return get(i)
        }
        CFRunLoopRunInMode(.defaultMode, 0.1, false)
    }
    return nil
}

var client = MIDIClientRef()
guard MIDIClientCreate("midi_probe" as CFString, nil, nil, &client) == noErr else {
    fatalError("cannot create MIDI client")
}
guard let source = find({ MIDIGetNumberOfSources() }, { MIDIGetSource($0) }),
    let destination = find({ MIDIGetNumberOfDestinations() }, { MIDIGetDestination($0) })
else { fatalError("no MIDI endpoints named \(name)") }

let received = DispatchSemaphore(value: 0)
var echoed: [[UInt8]] = []
let lock = NSLock()
var input = MIDIPortRef()
MIDIInputPortCreateWithBlock(client, "in" as CFString, &input) { list, _ in
    var packet = list.pointee.packet
    for _ in 0..<list.pointee.numPackets {
        let bytes = withUnsafeBytes(of: packet.data) { Array($0.prefix(Int(packet.length))) }
        // CoreMIDI sends the device SysEx of its own (MIDI-CI discovery), which comes back too.
        if let status = bytes.first, (0x80..<0xF0).contains(status) {
            lock.lock()
            echoed.append(bytes)
            lock.unlock()
            received.signal()
        }
        packet = MIDIPacketNext(&packet).pointee
    }
}
MIDIPortConnectSource(input, source, nil)
var output = MIDIPortRef()
MIDIOutputPortCreate(client, "out" as CFString, &output)

for round in 0..<rounds {
    let message: [UInt8] = [0x90, UInt8(round % 128), UInt8(1 + round % 127)]
    var list = MIDIPacketList()
    let packet = MIDIPacketListInit(&list)
    _ = MIDIPacketListAdd(&list, MemoryLayout<MIDIPacketList>.size, packet, 0, 3, message)
    MIDISend(output, destination, &list)
    guard received.wait(timeout: .now() + 2) == .success else {
        fatalError("round \(round): no echo")
    }
    lock.lock()
    let got = echoed.removeFirst()
    lock.unlock()
    guard got == message else { fatalError("round \(round): sent \(message), got \(got)") }
}
print("ok \(rounds)")
