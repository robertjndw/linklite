// SPDX-License-Identifier: GPL-2.0-or-later

import AppKit

/// Drives the menu bar symbol. It lives in an NSImageView rather than `button.image` because only
/// NSImageView supports symbol effects and animated symbol swaps.
@MainActor
final class StatusItemIcon {
    private enum Symbol: Equatable {
        case single, dual, working, attention

        var name: String {
            switch self {
            case .single: "display"
            case .dual, .working: "display.2"
            case .attention: "display.trianglebadge.exclamationmark"
            }
        }
    }

    /// Clicks must fall through to the NSStatusBarButton underneath, or the panel never opens.
    private final class PassthroughImageView: NSImageView {
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
    }

    private static let itemLength: CGFloat = 32

    private let button: NSStatusBarButton
    private let imageView = PassthroughImageView()
    private var symbol: Symbol?
    private var status: PanelModel.Status?
    private var label: String?
    private var effectRunning = false
    private var usingFallback = false

    init(statusItem: NSStatusItem) {
        button = statusItem.button ?? NSStatusBarButton()
        // With no image or title the variable length would collapse to nothing.
        statusItem.length = Self.itemLength
        button.image = nil
        imageView.translatesAutoresizingMaskIntoConstraints = false
        imageView.imageScaling = .scaleProportionallyDown
        imageView.symbolConfiguration = .init(pointSize: 14, weight: .regular)
        imageView.setAccessibilityElement(false)
        button.addSubview(imageView)
        NSLayoutConstraint.activate([
            imageView.centerXAnchor.constraint(equalTo: button.centerXAnchor),
            imageView.centerYAnchor.constraint(equalTo: button.centerYAnchor)
        ])
    }

    func update(status: PanelModel.Status, monitorCount: Int, hasProblem: Bool) {
        let next = Self.symbol(status: status, monitorCount: monitorCount, hasProblem: hasProblem)
        let description = Self.description(status: status, monitorCount: monitorCount, hasProblem: hasProblem)
        if description != label {
            label = description
            button.setAccessibilityLabel(description)
            button.toolTip = description
        }

        let reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        let symbolChanged = next != symbol
        let previousStatus = self.status
        self.status = status
        guard symbolChanged || effectRunning != (next == .working && !reduceMotion) else { return }

        if symbolChanged {
            guard let image = NSImage(systemSymbolName: next.name, accessibilityDescription: nil) else {
                showFallback()
                return
            }
            image.isTemplate = true
            usingFallback = false
            button.title = ""
            imageView.isHidden = false
            if symbol == nil || reduceMotion {
                imageView.image = image
            } else {
                imageView.setSymbolImage(image, contentTransition: .replace)
            }
            symbol = next
        }

        if next == .working && !reduceMotion {
            if !effectRunning {
                imageView.addSymbolEffect(.variableColor.iterative)
                effectRunning = true
            }
        } else if effectRunning {
            imageView.removeSymbolEffect(ofType: .variableColor)
            effectRunning = false
        }

        if !reduceMotion, status == .connected, previousStatus != nil, previousStatus != .connected {
            imageView.addSymbolEffect(.bounce, options: .nonRepeating)
        }
    }

    private func showFallback() {
        guard !usingFallback else { return }
        usingFallback = true
        symbol = nil
        imageView.isHidden = true
        button.title = "DL"
    }

    private static func symbol(status: PanelModel.Status, monitorCount: Int, hasProblem: Bool) -> Symbol {
        if status == .needsPermission || hasProblem { return .attention }
        switch status {
        case .connected: return monitorCount >= 2 ? .dual : .single
        case .starting, .searching, .retrying: return .working
        case .needsPermission: return .attention
        }
    }

    private static func description(status: PanelModel.Status, monitorCount: Int, hasProblem: Bool) -> String {
        let state: String
        switch status {
        case .connected:
            let displays = monitorCount == 1 ? "1 display" : "\(monitorCount) displays"
            state = hasProblem ? "Needs attention, \(displays)" : "Connected, \(displays)"
        case .starting: state = "Starting"
        case .searching: state = "Looking for your dock"
        case .retrying: state = "Reconnecting"
        case .needsPermission: state = "Screen Recording permission needed"
        }
        return "LinkLite, \(state)"
    }
}
