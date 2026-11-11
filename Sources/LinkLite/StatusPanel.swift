// SPDX-License-Identifier: GPL-2.0-or-later

import AppKit
import SwiftUI

/// The borderless glass panel that drops down from the menu-bar icon. It takes keyboard focus
/// without activating the app (so Esc and ⌘Q work) and closes on a click anywhere else.
@MainActor
final class StatusPanelController {
    private let model: PanelModel
    private let panel: Panel
    private let hosting: NSHostingController<PanelView?>
    /// Fresh for every presentation, so a reopen during the closing fade starts from scratch.
    private var presentation = PanelPresentation()
    private weak var anchor: NSStatusBarButton?
    private var monitors: [Any] = []
    var onShow: (() -> Void)?

    private(set) var isShown = false
    private var presentationID: UInt64 = 0
    private var resizeID: UInt64 = 0
    /// The latest smaller frame of a collapse in progress; set while a shrink is scheduled.
    private var pendingShrink: NSRect?

    init(model: PanelModel) {
        self.model = model
        // A hidden SwiftUI view can keep repeat-forever animations running. Build the view tree
        // only while presented, and release it after the closing fade finishes.
        hosting = NSHostingController(rootView: nil)
        // The hosting view keeps a fixed, screen-tall size, pinned to the top of a flipped container;
        // the window only clips it. Resizing the host itself moves its layers with the bottom edge
        // until SwiftUI catches up, and during a disclosure SwiftUI animates that correction, so the
        // header visibly drops and returns.
        hosting.sizingOptions = []
        panel = Panel()
        let container = TopAnchoredView()
        container.addSubview(hosting.view)
        panel.contentView = container
        panel.styleMask = [.borderless, .nonactivatingPanel]
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.level = .statusBar
        panel.collectionBehavior = [.canJoinAllSpaces, .transient, .ignoresCycle]
        panel.isMovable = false
        panel.hidesOnDeactivate = false
        hosting.view.wantsLayer = true
        hosting.view.layer?.backgroundColor = .clear
    }

    func toggle(from button: NSStatusBarButton) {
        isShown ? hide() : show(from: button)
    }

    func show(from button: NSStatusBarButton) {
        guard !isShown else { return }
        isShown = true
        presentationID &+= 1
        anchor = button
        onShow?()
        // The view springs itself in from .entering once it appears.
        presentation = PanelPresentation()
        hosting.rootView = PanelView(model: model, presentation: presentation) { [weak self] size in
            self?.place(size: size)
        }
        place(size: hosting.sizeThatFits(in: .zero))
        panel.alphaValue = 0
        panel.makeKeyAndOrderFront(nil)
        // Reduce Motion: appear and disappear at once; the glass inside only fades.
        let reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        NSAnimationContext.runAnimationGroup { context in
            context.duration = reduceMotion ? 0 : 0.16
            panel.animator().alphaValue = 1
        }
        button.highlight(true)

        let outside = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] _ in
            MainActor.assumeIsolated { self?.hide() }
        }
        let escape = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard event.keyCode == 53 else { return event }
            MainActor.assumeIsolated { self?.hide() }
            return nil
        }
        monitors = [outside, escape].compactMap { $0 }
    }

    func hide() {
        guard isShown else { return }
        isShown = false
        pendingShrink = nil
        presentationID &+= 1
        let closingPresentation = presentationID
        monitors.forEach(NSEvent.removeMonitor)
        monitors = []
        anchor?.highlight(false)
        // The glass fades and settles slightly inward while the window fades around it; both take
        // the same 0.12 s, so the tree is released only after the glass is gone.
        presentation.phase = .leaving
        NSAnimationContext.runAnimationGroup { context in
            context.duration = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion ? 0 : 0.12
            panel.animator().alphaValue = 0
        } completionHandler: { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.presentationID == closingPresentation else { return }
                self.panel.orderOut(nil)
                self.hosting.rootView = nil
            }
        }
    }

    /// The glass centred under the icon, 6 pt below the menu bar, kept inside the screen. The window
    /// extends past the glass by the shadow margin on every side.
    private func place(size: NSSize) {
        guard isShown, let button = anchor, let window = button.window else { return }
        let icon = window.convertToScreen(button.convert(button.bounds, to: nil))
        let margin = PanelView.shadowMargin
        let visible = (window.screen ?? NSScreen.main)?.visibleFrame ?? .zero
        var x = icon.midX - size.width / 2
        x = min(max(x, visible.minX + 8 - margin), visible.maxX - size.width - 8 + margin)
        let top = (icon.minY - 6 + margin).rounded()
        let frame = NSRect(x: x.rounded(), y: top - size.height, width: size.width, height: size.height)
        let host = NSSize(width: size.width, height: max(visible.height, size.height))
        if hosting.view.frame.size != host {
            hosting.view.frame = NSRect(origin: .zero, size: host)
        }
        if panel.isVisible, frame.height < panel.frame.height,
           !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion {
            // Clip the window only after the glass has finished collapsing inside it: the layout
            // spring's settle time after the latest smaller size. Clipping late only leaves
            // transparent space; clipping early would cut off glass that is still animating.
            pendingShrink = frame
            resizeID &+= 1
            let pending = resizeID
            DispatchQueue.main.asyncAfter(deadline: .now() + PanelMotion.settleDuration) { [weak self] in
                guard let self, self.isShown, self.resizeID == pending, let frame = self.pendingShrink else { return }
                self.pendingShrink = nil
                // A reversal toggled just now reports its new size only on SwiftUI's next pass;
                // don't clip content that has already started growing again.
                guard self.hosting.sizeThatFits(in: .zero).height <= frame.height else { return }
                self.panel.setFrame(frame, display: true)
            }
        } else {
            // Growing takes the final size at once; the extra area is transparent while the glass
            // expands into it. This, or a reversal back to the current size, cancels a pending shrink.
            pendingShrink = nil
            resizeID &+= 1
            guard panel.frame != frame else { return }
            panel.setFrame(frame, display: true)
        }
    }

    private final class TopAnchoredView: NSView {
        override var isFlipped: Bool { true }
    }

    private final class Panel: NSPanel {
        override var canBecomeKey: Bool { true }
    }
}
