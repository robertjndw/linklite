import Foundation

public enum LinkAssessment: String, Equatable {
    case pass = "PASS"
    case fail = "FAIL"
    case unknown = "UNKNOWN"
}

public struct USBDeviceReport: Equatable {
    public var productName: String?
    public var vendorName: String?
    public var serialNumber: String?
    public var vendorID: Int
    public var productID: Int
    public var usbLinkSpeedBitsPerSecond: Int64?
    public var exclusiveOwner: String?

    public init(
        productName: String?,
        vendorName: String?,
        serialNumber: String?,
        vendorID: Int,
        productID: Int,
        usbLinkSpeedBitsPerSecond: Int64?,
        exclusiveOwner: String?
    ) {
        self.productName = productName
        self.vendorName = vendorName
        self.serialNumber = serialNumber
        self.vendorID = vendorID
        self.productID = productID
        self.usbLinkSpeedBitsPerSecond = usbLinkSpeedBitsPerSecond
        self.exclusiveOwner = exclusiveOwner
    }

    public var linkSpeedMbps: Int? {
        usbLinkSpeedBitsPerSecond.map { Int($0 / 1_000_000) }
    }

    public var assessment: LinkAssessment {
        assessLinkSpeedMbps(linkSpeedMbps)
    }

    public var vidpid: String {
        "\(hex4(vendorID)):\(hex4(productID))"
    }
}

public func assessLinkSpeedMbps(_ mbps: Int?) -> LinkAssessment {
    guard let mbps else { return .unknown }
    return mbps >= 5_000 ? .pass : .fail
}

public func parseHexID(_ value: String) -> Int? {
    let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
    let raw = trimmed.lowercased().hasPrefix("0x") ? String(trimmed.dropFirst(2)) : trimmed
    guard !raw.isEmpty, raw.count <= 4, let parsed = Int(raw, radix: 16) else {
        return nil
    }
    return (0...0xffff).contains(parsed) ? parsed : nil
}

public func formatHuman(_ devices: [USBDeviceReport]) -> String {
    if devices.isEmpty {
        return "No matching DisplayLink USB devices found."
    }

    return devices.map { device in
        let speed = device.linkSpeedMbps.map { "\($0) Mbps" } ?? "unknown"
        let owner = device.exclusiveOwner?.isEmpty == false ? device.exclusiveOwner! : "none reported"
        return """
        \(device.productName ?? "Unknown USB device") (\(device.vidpid))
          Vendor: \(device.vendorName ?? "unknown")
          Serial: \(device.serialNumber ?? "unknown")
          USB link speed: \(speed)
          Exclusive owner: \(owner)
          Dual-4K transport preflight: \(device.assessment.rawValue)
        """
    }.joined(separator: "\n\n")
}

public func jsonData(_ devices: [USBDeviceReport]) throws -> Data {
    let rows = devices.map { device -> [String: Any] in
        [
            "productName": device.productName ?? NSNull(),
            "vendorName": device.vendorName ?? NSNull(),
            "serialNumber": device.serialNumber ?? NSNull(),
            "vendorID": hex4(device.vendorID),
            "productID": hex4(device.productID),
            "vidpid": device.vidpid,
            "usbLinkSpeedMbps": device.linkSpeedMbps ?? NSNull(),
            "usbLinkSpeedBitsPerSecond": device.usbLinkSpeedBitsPerSecond ?? NSNull(),
            "exclusiveOwner": device.exclusiveOwner ?? NSNull(),
            "assessment": device.assessment.rawValue
        ]
    }
    return try JSONSerialization.data(withJSONObject: rows, options: [.prettyPrinted, .sortedKeys])
}

public func hex4(_ value: Int) -> String {
    String(format: "0x%04x", value)
}

// MARK: - Configuration descriptor layout (Gate 1, read-only)

public enum EndpointTransfer: String, Equatable {
    case control, isochronous, bulk, interrupt
}

public struct EndpointLayout: Equatable {
    public var address: Int
    public var transfer: EndpointTransfer
    public var maxPacketSize: Int
    public var interval: Int
    public var maxBurst: Int?

    public init(address: Int, transfer: EndpointTransfer, maxPacketSize: Int, interval: Int, maxBurst: Int? = nil) {
        self.address = address
        self.transfer = transfer
        self.maxPacketSize = maxPacketSize
        self.interval = interval
        self.maxBurst = maxBurst
    }

    public var isIn: Bool { address & 0x80 != 0 }
}

public struct InterfaceLayout: Equatable {
    public var number: Int
    public var alternateSetting: Int
    public var interfaceClass: Int
    public var subclass: Int
    public var interfaceProtocol: Int
    public var endpoints: [EndpointLayout]

    public init(number: Int, alternateSetting: Int, interfaceClass: Int, subclass: Int, interfaceProtocol: Int, endpoints: [EndpointLayout] = []) {
        self.number = number
        self.alternateSetting = alternateSetting
        self.interfaceClass = interfaceClass
        self.subclass = subclass
        self.interfaceProtocol = interfaceProtocol
        self.endpoints = endpoints
    }

    public var role: String {
        switch (interfaceClass, subclass, interfaceProtocol) {
        case (0xff, _, _): "vendor-specific (DisplayLink)"
        case (0xfe, 0x01, _): "DFU firmware update - never touch"
        case (0x01, _, _): "USB audio"
        case (0x02, 0x0d, _): "CDC NCM ethernet control"
        case (0x0a, _, _): "CDC data"
        default: "other"
        }
    }
}

public enum DescriptorParseError: Error, Equatable {
    case truncated(offset: Int)
    case notConfiguration
}

/// Parses a full USB configuration descriptor (wTotalLength bytes) into interfaces and endpoints.
public func parseConfigurationDescriptor(_ bytes: [UInt8]) throws -> [InterfaceLayout] {
    guard bytes.count >= 9, bytes[0] >= 9, bytes[1] == 0x02 else {
        throw DescriptorParseError.notConfiguration
    }
    let total = Int(bytes[2]) | Int(bytes[3]) << 8
    guard total >= Int(bytes[0]) else { throw DescriptorParseError.truncated(offset: 0) }
    guard total <= bytes.count else { throw DescriptorParseError.truncated(offset: bytes.count) }
    var interfaces: [InterfaceLayout] = []
    var offset = 0
    while offset < total {
        let length = Int(bytes[offset])
        guard length >= 2, offset + length <= total else { throw DescriptorParseError.truncated(offset: offset) }
        let d = bytes[offset..<(offset + length)]
        let b = { (i: Int) in Int(d[d.startIndex + i]) }
        switch b(1) {
        case 0x04 where length >= 9:
            interfaces.append(InterfaceLayout(number: b(2), alternateSetting: b(3), interfaceClass: b(5), subclass: b(6), interfaceProtocol: b(7)))
        case 0x05 where length >= 7 && !interfaces.isEmpty:
            let transfer: EndpointTransfer = [.control, .isochronous, .bulk, .interrupt][b(3) & 0x03]
            interfaces[interfaces.count - 1].endpoints.append(
                EndpointLayout(address: b(2), transfer: transfer, maxPacketSize: (b(4) | b(5) << 8) & 0x7ff, interval: b(6))
            )
        case 0x30 where length >= 6:
            if var last = interfaces.last, !last.endpoints.isEmpty {
                last.endpoints[last.endpoints.count - 1].maxBurst = b(2) + 1
                interfaces[interfaces.count - 1] = last
            }
        default:
            break
        }
        offset += length
    }
    return interfaces
}

/// The dock's own statement of what it is: a 16-byte vendor descriptor (type 0x40) inside the
/// configuration descriptor, `[len, 0x40, major, minor, patch, ..., name(8)]`. The engine places a
/// dock by this name, not by product ID, so it is what decides whether a dock is supported.
public struct DockIdentity: Equatable {
    public var firmware: (major: Int, minor: Int, patch: Int)
    public var platform: String

    public init(firmware: (major: Int, minor: Int, patch: Int), platform: String) {
        self.firmware = firmware
        self.platform = platform
    }

    public static func == (a: DockIdentity, b: DockIdentity) -> Bool {
        a.firmware == b.firmware && a.platform == b.platform
    }

    /// Mirrors `Family::from_identity` and `profile::for_family` in vendor/dlcore, which stay the
    /// source of truth; dlprobe deliberately does not link the engine, so the names are restated.
    public var family: (name: String, supported: Bool)? {
        switch platform {
        case "NavaDock": ("Navarro (DL-7400)", true)
        case "Ridge", "RidgeDoc": ("Ridge (DL-6xxx)", true)
        case "Ella", "EllaDock": ("Ella (DL-3x00)", true)
        case "Firefly", "FflyMoni": ("Firefly monitor", false)
        default: nil
        }
    }

    public var summary: String {
        let version = "\(firmware.major).\(firmware.minor).\(firmware.patch)"
        guard let family else {
            return "Identity: \"\(platform)\" firmware \(version) - unrecognised family, not supported"
        }
        let verdict = family.supported ? "supported" : "not supported"
        return "Identity: \"\(platform)\" firmware \(version) - \(family.name), \(verdict)"
    }
}

/// The identity descriptor from a full configuration descriptor, or nil when the dock has none.
public func parseDockIdentity(_ bytes: [UInt8]) -> DockIdentity? {
    var offset = 0
    while offset + 2 <= bytes.count {
        let length = Int(bytes[offset])
        guard length >= 2, offset + length <= bytes.count else { return nil }
        if bytes[offset + 1] == 0x40, length >= 16 {
            let d = Array(bytes[offset..<(offset + 16)])
            // The name is padded with NULs or spaces, matching `Identity::platform` in dlcore-usb.
            let name = d[8..<16].prefix { $0 != 0 && $0 != 0x20 }
            let platform = String(decoding: name, as: UTF8.self)
            return DockIdentity(firmware: (Int(d[2]), Int(d[3]), Int(d[4])), platform: platform)
        }
        offset += length
    }
    return nil
}

public func formatInterfaces(_ interfaces: [InterfaceLayout]) -> String {
    interfaces.map { interface in
        let header = "Interface \(interface.number) alt \(interface.alternateSetting): class \(hex2(interface.interfaceClass))/\(hex2(interface.subclass))/\(hex2(interface.interfaceProtocol)) - \(interface.role)"
        let endpoints = interface.endpoints.map { ep in
            let burst = ep.maxBurst.map { " burst \($0)" } ?? ""
            return "    EP \(hex2(ep.address)) \(ep.isIn ? "IN " : "OUT") \(ep.transfer.rawValue) max \(ep.maxPacketSize)\(burst) interval \(ep.interval)"
        }
        return ([header] + endpoints).joined(separator: "\n")
    }.joined(separator: "\n")
}

public func hex2(_ value: Int) -> String {
    String(format: "0x%02x", value)
}
