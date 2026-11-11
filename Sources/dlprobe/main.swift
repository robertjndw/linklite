import DLProbeCore
import Foundation
import IOKit
import IOUSBHost

struct Options {
    var json = false
    var descriptors = false
    var productID = 0x600f
}

func usage() -> String {
    """
    usage: dlprobe [--json] [--descriptors] [--product HEX]

    Read-only preflight for DisplayLink USB devices. Default product filter: 0x600f.
    --descriptors prints the cached configuration descriptor layout (no interface claim, no vendor requests).
    """
}

func parseOptions(_ args: [String]) throws -> Options {
    var options = Options()
    var index = 0
    while index < args.count {
        switch args[index] {
        case "--json":
            options.json = true
        case "--descriptors":
            options.descriptors = true
        case "--product":
            index += 1
            guard index < args.count, let product = parseHexID(args[index]) else {
                throw CLIError.invalidArgument("--product requires a hex value like 600f")
            }
            options.productID = product
        case "-h", "--help":
            throw CLIError.help
        default:
            throw CLIError.invalidArgument("unknown argument: \(args[index])")
        }
        index += 1
    }
    return options
}

enum CLIError: Error, CustomStringConvertible {
    case help
    case invalidArgument(String)

    var description: String {
        switch self {
        case .help:
            usage()
        case .invalidArgument(let message):
            "\(message)\n\n\(usage())"
        }
    }
}

func matchingDisplayLinkServices(productID: Int) -> [io_service_t] {
    let matching = IOServiceMatching("IOUSBHostDevice")
    var iterator: io_iterator_t = 0
    guard IOServiceGetMatchingServices(kIOMainPortDefault, matching, &iterator) == KERN_SUCCESS else {
        return []
    }
    defer { IOObjectRelease(iterator) }

    var services: [io_service_t] = []
    while true {
        let service = IOIteratorNext(iterator)
        if service == 0 { break }
        guard let properties = properties(for: service),
              intValue(properties["idVendor"]) == 0x17e9,
              intValue(properties["idProduct"]) == productID else {
            IOObjectRelease(service)
            continue
        }
        services.append(service)
    }
    return services
}

func enumerateDisplayLinkDevices(productID: Int) -> [USBDeviceReport] {
    var devices: [USBDeviceReport] = []
    for service in matchingDisplayLinkServices(productID: productID) {
        defer { IOObjectRelease(service) }
        guard let properties = properties(for: service),
              let vendor = intValue(properties["idVendor"]),
              let product = intValue(properties["idProduct"]) else {
            continue
        }

        devices.append(USBDeviceReport(
            productName: stringValue(properties["USB Product Name"]) ?? stringValue(properties["Product Name"]),
            vendorName: stringValue(properties["USB Vendor Name"]) ?? stringValue(properties["Vendor Name"]),
            serialNumber: stringValue(properties["USB Serial Number"]) ?? stringValue(properties["Serial Number"]),
            vendorID: vendor,
            productID: product,
            usbLinkSpeedBitsPerSecond: int64Value(properties["UsbLinkSpeed"]),
            exclusiveOwner: stringValue(properties["UsbExclusiveOwner"])
        ))
    }
    return devices
}

/// Reads the current configuration descriptor held by the host stack. Standard descriptor read only.
func readConfigurationDescriptor(service: io_service_t) throws -> [UInt8] {
    let device = try IOUSBHostDevice(__ioService: service, options: [], queue: nil, interestHandler: nil)
    defer { device.destroy() }
    guard let config = device.configurationDescriptor else {
        throw CLIError.invalidArgument("device has no active configuration")
    }
    let total = Int(config.pointee.wTotalLength)
    return config.withMemoryRebound(to: UInt8.self, capacity: total) { Array(UnsafeBufferPointer(start: $0, count: total)) }
}

func properties(for service: io_service_t) -> [String: Any]? {
    var rawProperties: Unmanaged<CFMutableDictionary>?
    let result = IORegistryEntryCreateCFProperties(service, &rawProperties, kCFAllocatorDefault, 0)
    guard result == KERN_SUCCESS, let rawProperties else { return nil }
    return rawProperties.takeRetainedValue() as NSDictionary as? [String: Any]
}

func intValue(_ value: Any?) -> Int? {
    if let value = value as? Int { return value }
    if let value = value as? NSNumber { return value.intValue }
    if let value = value as? String { return Int(value) }
    return nil
}

func int64Value(_ value: Any?) -> Int64? {
    if let value = value as? Int64 { return value }
    if let value = value as? Int { return Int64(value) }
    if let value = value as? NSNumber { return value.int64Value }
    if let value = value as? String { return Int64(value) }
    return nil
}

func stringValue(_ value: Any?) -> String? {
    if let value = value as? String { return value }
    if let value = value as? NSNumber { return value.stringValue }
    return nil
}

do {
    let options = try parseOptions(Array(CommandLine.arguments.dropFirst()))
    if options.descriptors {
        let services = matchingDisplayLinkServices(productID: options.productID)
        if services.isEmpty { print("No matching DisplayLink USB devices found.") }
        for service in services {
            defer { IOObjectRelease(service) }
            let config = try readConfigurationDescriptor(service: service)
            print(parseDockIdentity(config)?.summary ?? "Identity: no identity descriptor; only product-ID quirks can place this dock")
            print(formatInterfaces(try parseConfigurationDescriptor(config)))
        }
        exit(0)
    }
    let devices = enumerateDisplayLinkDevices(productID: options.productID)
    if options.json {
        let data = try jsonData(devices)
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data("\n".utf8))
    } else {
        print(formatHuman(devices))
    }
} catch let error as CLIError {
    let isHelp: Bool
    if case .help = error {
        isHelp = true
    } else {
        isHelp = false
    }
    let stream = isHelp ? FileHandle.standardOutput : FileHandle.standardError
    stream.write(Data("\(error.description)\n".utf8))
    exit(isHelp ? 0 : 2)
} catch {
    FileHandle.standardError.write(Data("dlprobe: \(error)\n".utf8))
    exit(1)
}
