// SPDX-License-Identifier: GPL-2.0-or-later

import AppKit
@preconcurrency import ScreenCaptureKit
import SwiftUI

/// What the menu-bar panel shows. AppDelegate refreshes it from DisplayHost, SMAppService and
/// UpdateManager, and supplies the actions.
@MainActor
final class PanelModel: ObservableObject {
    enum Status: Equatable {
        case starting, searching, connected, retrying, needsPermission
    }

    struct Monitor: Identifiable, Equatable {
        let id: UInt8
        let name: String
        let detail: String
        let problem: String?
        /// The virtual display macOS shows for this monitor, once it exists.
        let displayID: CGDirectDisplayID?
        /// Global bounds from CGDisplayBounds, used to draw the arrangement as System Settings does.
        let frame: CGRect?
    }

    @Published var status: Status = .starting
    @Published var message = ""
    @Published var monitors: [Monitor] = []
    @Published var openAtLogin = false
    @Published var loginNeedsApproval = false
    @Published var updatesAvailable = false
    @Published var automaticUpdates = false
    @Published var canCheckForUpdates = false

    var toggleOpenAtLogin: () -> Void = {}
    var toggleAutomaticUpdates: () -> Void = {}
    var checkForUpdates: () -> Void = {}
    var openScreenRecordingSettings: () -> Void = {}
    var openDisplaysSettings: () -> Void = {}
    var quit: () -> Void = {}
}

// MARK: - Motion

/// Every curve the panel uses, shared with the native window, which clips itself only after a
/// shrink has settled.
@MainActor
enum PanelMotion {
    /// Height changes. No bounce: the window grows at once but clips only after a shrink settles,
    /// and an overshooting glass edge would read as a glitch against the transparent margin.
    static let layoutSpring = Spring(duration: 0.32, bounce: 0)
    static let layout = Animation.spring(layoutSpring)
    /// How long the layout spring takes to come to rest, not just its perceptual duration.
    static let settleDuration: TimeInterval = layoutSpring.settlingDuration

    static let open = Animation.spring(duration: 0.32, bounce: 0.18)
    static let close = Animation.easeOut(duration: 0.12)
    static let attach = Animation.spring(duration: 0.5, bounce: 0.32)
    static let powerOn = Animation.easeOut(duration: 0.55).delay(0.16)
    static let hover = Animation.easeOut(duration: 0.08)
}

/// Lets StatusPanelController play the close animation on a view tree it is about to release.
@MainActor
final class PanelPresentation: ObservableObject {
    enum Phase {
        case entering, shown, leaving
    }

    @Published var phase: Phase

    init(phase: Phase = .entering) {
        self.phase = phase
    }
}

// MARK: - Panel

struct PanelView: View {
    @ObservedObject var model: PanelModel
    @ObservedObject var presentation: PanelPresentation
    let onSizeChange: (CGSize) -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Namespace private var displayMorph

    static let shadowMargin: CGFloat = 28
    static let width: CGFloat = 300
    /// Text and artwork line up on this inset; menu rows sit closer to the edge so their highlight
    /// reaches nearly across, as in an NSMenu.
    static let contentInset: CGFloat = 14
    static let rowInset: CGFloat = 6

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            PanelHeader(model: model)
                .padding(.horizontal, Self.contentInset)
                .padding(.top, 12)
                .padding(.bottom, 10)
            hero
                .padding(.horizontal, Self.contentInset)
                .padding(.bottom, 8)
            PanelSeparator()
            toggles
            PanelSeparator()
            VStack(spacing: 0) {
                if model.updatesAvailable {
                    Button("Check for Updates…", action: model.checkForUpdates)
                        .disabled(!model.canCheckForUpdates)
                        .transition(.opacity)
                }
                Button("Displays Settings…", action: model.openDisplaysSettings)
            }
            .buttonStyle(MenuRowButtonStyle())
            .padding(.horizontal, Self.rowInset)
            PanelSeparator()
            Button(action: model.quit) {
                HStack {
                    Text("Quit LinkLite")
                    Spacer(minLength: 12)
                    Text("⌘Q").foregroundStyle(.secondary)
                }
            }
            .buttonStyle(MenuRowButtonStyle())
            .keyboardShortcut("q", modifiers: .command)
            .padding(.horizontal, Self.rowInset)
        }
        .padding(.bottom, Self.rowInset)
        .frame(width: Self.width)
        // Leaving content keeps its old place while the glass shrinks; keep it inside the glass.
        .clipShape(RoundedRectangle(cornerRadius: GlassPanel.cornerRadius, style: .continuous))
        .glassPanel()
        // The window is transparent and shadowless; this shadow follows the rounded glass, and the
        // margin keeps it from being clipped. StatusPanelController offsets the window by the margin.
        .shadow(color: .black.opacity(0.28), radius: 18, y: 8)
        // Scale and fade are transforms, so the size reported below (and the window) never moves.
        // Anchored at the top so the glass seems to come out of the menu bar icon.
        .scaleEffect(presentationScale, anchor: .top)
        .opacity(presentation.phase == .shown ? 1 : 0)
        .animation(presentationAnimation, value: presentation.phase)
        // Status and monitor changes arrive without a transaction. Animate them here, outside the
        // glass, so the glass and its shadow resize on the same curve as the content inside.
        .animation(layoutAnimation, value: model.status)
        .animation(layoutAnimation, value: model.message)
        .animation(layoutAnimation, value: model.monitors)
        .animation(layoutAnimation, value: model.loginNeedsApproval)
        .animation(layoutAnimation, value: model.updatesAvailable)
        .padding(Self.shadowMargin)
        .fixedSize()
        // The window grows to the final size at once and shrinks once the spring settles. Keep the
        // content top-aligned so the glass animates downward from a fixed top edge.
        .onGeometryChange(for: CGSize.self) { $0.size } action: { onSizeChange($0) }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .onAppear { presentation.phase = .shown }
    }

    private var layoutAnimation: Animation? {
        reduceMotion ? nil : PanelMotion.layout
    }

    private var presentationAnimation: Animation {
        switch presentation.phase {
        case .shown: reduceMotion ? .easeOut(duration: 0.15) : PanelMotion.open
        case .entering, .leaving: PanelMotion.close
        }
    }

    private var presentationScale: CGFloat {
        guard !reduceMotion else { return 1 }
        switch presentation.phase {
        case .entering: return 0.94
        case .shown: return 1
        case .leaving: return 0.98
        }
    }

    /// Under the header: the permission prompt, the searching placeholder, or the display map.
    @ViewBuilder
    private var hero: some View {
        if model.status == .needsPermission {
            PermissionCallout(action: model.openScreenRecordingSettings)
                .transition(.opacity)
        } else if model.monitors.isEmpty {
            SearchingPlaceholder(caption: placeholderCaption, isSearching: isSearching, namespace: displayMorph)
                .transition(.opacity)
        } else {
            ArrangementMap(
                monitors: model.monitors,
                namespace: displayMorph,
                action: model.openDisplaysSettings
            )
            .transition(.opacity)
        }
    }

    private var isSearching: Bool {
        switch model.status {
        case .starting, .searching, .retrying: true
        case .connected, .needsPermission: false
        }
    }

    /// Only when there is something to say beyond the header's status.
    private var placeholderCaption: String? {
        switch model.status {
        case .starting, .needsPermission:
            return nil
        case .connected:
            return "Connect a display to your dock."
        case .searching where model.message.isEmpty || model.message.hasPrefix("Looking for"):
            return "Connect your DisplayLink dock."
        case .searching, .retrying:
            // The engine's own words for anything unusual, such as the dock being in use elsewhere.
            return model.message.isEmpty ? nil : model.message
        }
    }

    private var toggles: some View {
        VStack(spacing: 0) {
            ToggleRow(
                title: "Open at Login",
                subtitle: model.loginNeedsApproval ? "Approve LinkLite in Login Items" : nil,
                isOn: model.openAtLogin,
                action: model.toggleOpenAtLogin
            )
            if model.updatesAvailable {
                ToggleRow(
                    title: "Install Updates Automatically",
                    subtitle: nil,
                    isOn: model.automaticUpdates,
                    action: model.toggleAutomaticUpdates
                )
                .transition(.opacity)
            }
        }
        .padding(.horizontal, Self.contentInset)
        .padding(.vertical, 2)
    }
}

// MARK: - Header

private struct PanelHeader: View {
    @ObservedObject var model: PanelModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(alignment: .center, spacing: 8) {
            Text("LinkLite")
                .font(.system(size: 13, weight: .semibold))
            Spacer(minLength: 8)
            HStack(spacing: 6) {
                Text(title)
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .contentTransition(.opacity)
                Image(systemName: "circle.fill")
                    .font(.system(size: 6))
                    .foregroundStyle(dotColor)
                    .symbolEffect(.pulse, options: .repeating, isActive: isSearching && !reduceMotion)
                    .accessibilityHidden(true)
            }
        }
        .accessibilityElement(children: .combine)
    }

    private var title: String {
        switch model.status {
        case .starting: "Starting"
        case .searching: "Looking for Your Dock"
        case .connected: "Connected"
        case .retrying: "Reconnecting"
        case .needsPermission: "Permission Needed"
        }
    }

    private var isSearching: Bool {
        model.status == .starting || model.status == .searching || model.status == .retrying
    }

    private var dotColor: Color {
        switch model.status {
        case .connected:
            model.monitors.contains { $0.problem != nil } ? .orange : .accentColor
        case .needsPermission: .red
        case .retrying: .orange
        // Waiting for a dock is normal, not a warning.
        case .starting, .searching: .secondary
        }
    }
}

// MARK: - Arrangement

/// The dock's monitors laid out like System Settings › Displays, each with a snapshot of its screen
/// and its name and mode underneath. There is no separate list, so each tile is also the button
/// and the VoiceOver element.
private struct ArrangementMap: View {
    let monitors: [PanelModel.Monitor]
    let namespace: Namespace.ID
    let action: () -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var hoveredID: UInt8?

    static let width = PanelView.width - 2 * PanelView.contentInset
    /// Side by side screens use the short end; a stacked arrangement may grow toward the tall end
    /// so its screens stay big enough to caption.
    static let heights: ClosedRange<CGFloat> = 84...156
    static let captionBand: CGFloat = 32
    /// What the placeholder reserves, so the panel does not jump when the first display arrives.
    static var height: CGFloat { heights.lowerBound + captionBand }

    struct Placement: Identifiable {
        let monitor: PanelModel.Monitor
        let rect: CGRect
        /// Another display sits right under this one, so its caption goes inside instead.
        let captionInside: Bool
        /// Wider than the screen when nothing else shares its caption line.
        let captionWidth: CGFloat
        var id: UInt8 { monitor.id }
    }

    var body: some View {
        let (placements, box) = Self.layout(monitors)
        ZStack(alignment: .topLeading) {
            ForEach(placements) { placement in
                item(placement, box: box, isFirst: placement.id == monitors.first?.id)
                    .transition(attachTransition(for: placement.rect, in: box))
            }
        }
        .frame(width: box.width, height: box.height, alignment: .topLeading)
    }

    private func item(_ placement: Placement, box: CGSize, isFirst: Bool) -> some View {
        let rect = placement.rect
        let monitor = placement.monitor
        let isHovered = hoveredID == placement.id
        return ZStack(alignment: .topLeading) {
            MapDisplay(monitor: monitor, isHovered: isHovered)
                .frame(width: rect.width, height: rect.height)
                .overlay(alignment: .bottom) {
                    if placement.captionInside {
                        Caption(monitor: monitor, onImage: true)
                            .padding(.horizontal, 4)
                            .padding(.vertical, 3)
                            .frame(maxWidth: .infinity)
                            .background(LinearGradient(colors: [.clear, .black.opacity(0.55)],
                                                       startPoint: .top, endPoint: .bottom))
                    }
                }
                .clipShape(RoundedRectangle(cornerRadius: 4, style: .continuous))
                // The searching placeholder turns into the first display that shows up.
                .matchedGeometryEffect(id: isFirst ? "first" : "display-\(placement.id)", in: namespace)
                .position(x: rect.midX, y: rect.midY)
            if !placement.captionInside {
                Caption(monitor: monitor, onImage: false)
                    .frame(width: placement.captionWidth)
                    .position(x: rect.midX, y: rect.maxY + Self.captionBand / 2 + 1)
            }
        }
        .frame(width: box.width, height: box.height, alignment: .topLeading)
        .contentShape(Rectangle().path(in: rect.union(
            CGRect(x: rect.minX, y: rect.maxY, width: rect.width, height: Self.captionBand))))
        .onHover { inside in
            if inside {
                hoveredID = placement.id
            } else if hoveredID == placement.id {
                hoveredID = nil
            }
        }
        .onTapGesture(perform: action)
        .animation(PanelMotion.hover, value: isHovered)
        .accessibilityElement(children: .ignore)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(monitor.name)
        .accessibilityValue(monitor.problem ?? monitor.detail)
        .accessibilityHint("Opens Displays settings")
        .accessibilityAction { action() }
    }

    /// A display added while the map is open grows in from its centre, then flashes briefly as if
    /// powering on.
    private func attachTransition(for rect: CGRect, in box: CGSize) -> AnyTransition {
        guard !reduceMotion else { return .opacity }
        let center = UnitPoint(x: rect.midX / box.width, y: rect.midY / box.height)
        let grow = AnyTransition.scale(scale: 0.6, anchor: center)
            .combined(with: .opacity)
            .animation(PanelMotion.attach)
        let flash = AnyTransition.modifier(active: PowerOn(brightness: 0.3), identity: PowerOn(brightness: 0))
            .animation(PanelMotion.powerOn)
        return .asymmetric(insertion: grow.combined(with: flash), removal: grow)
    }

    /// Normalises the displays' global bounds into the map, keeping their proportions and
    /// arrangement. Without bounds for every display, falls back to a row of 16:9 screens.
    static func layout(_ monitors: [PanelModel.Monitor]) -> (placements: [Placement], box: CGSize) {
        guard !monitors.isEmpty else { return ([], CGSize(width: width, height: height)) }
        let known = monitors.compactMap(\.frame).filter { $0.width > 0 && $0.height > 0 }
        let frames: [CGRect] = known.count == monitors.count
            ? known
            : monitors.indices.map { CGRect(x: CGFloat($0) * 16, y: 0, width: 16, height: 9) }
        let union = frames.dropFirst().reduce(frames[0]) { $0.union($1) }
        let mapHeight = min(max(union.height * width / union.width, heights.lowerBound), heights.upperBound)
        let scale = min(width / union.width, mapHeight / union.height)
        let origin = CGPoint(
            x: (width - union.width * scale) / 2,
            y: (mapHeight - union.height * scale) / 2
        )
        // Displays touch edge to edge in global space; a gap keeps them apart as in Settings.
        let rects = frames.map { frame in
            CGRect(
                x: origin.x + (frame.minX - union.minX) * scale,
                y: origin.y + (frame.minY - union.minY) * scale,
                width: frame.width * scale,
                height: frame.height * scale
            ).insetBy(dx: 2, dy: 2)
        }
        let placements = zip(monitors, rects).enumerated().map { index, pair in
            let rect = pair.1
            let others = rects.enumerated().filter { $0.offset != index }.map(\.element)
            let band = CGRect(x: rect.minX, y: rect.maxY, width: rect.width, height: captionBand)
            let blocked = others.contains { $0.intersects(band) }
            // Another screen whose caption or picture shares this line limits how wide this
            // caption may spread; alone on its line, it can use the whole map.
            let fullLine = CGRect(x: 0, y: rect.maxY, width: width, height: captionBand)
            let crowded = others.contains { other in
                other.intersects(fullLine)
                    || CGRect(x: other.minX, y: other.maxY, width: other.width, height: captionBand).intersects(fullLine)
            }
            return Placement(monitor: pair.0, rect: rect, captionInside: blocked,
                             captionWidth: crowded ? max(rect.width, 96) : width)
        }
        return (placements, CGSize(width: width, height: mapHeight + captionBand))
    }
}

/// Name over mode, or over the problem in orange.
private struct Caption: View {
    let monitor: PanelModel.Monitor
    /// Drawn over the snapshot rather than the glass, so it needs to be white.
    let onImage: Bool

    var body: some View {
        VStack(spacing: 1) {
            Text(monitor.name)
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(onImage ? AnyShapeStyle(.white) : AnyShapeStyle(.primary))
            Text(monitor.problem ?? monitor.detail)
                .font(.system(size: 10).monospacedDigit())
                .foregroundStyle(monitor.problem != nil ? AnyShapeStyle(.orange)
                                 : onImage ? AnyShapeStyle(.white.opacity(0.8)) : AnyShapeStyle(.secondary))
        }
        .lineLimit(1)
        .truncationMode(.middle)
        .multilineTextAlignment(.center)
    }
}

private struct PowerOn: ViewModifier {
    let brightness: Double

    func body(content: Content) -> some View {
        content.brightness(brightness)
    }
}

private struct MapDisplay: View {
    let monitor: PanelModel.Monitor
    let isHovered: Bool
    @State private var snapshot: CGImage?
    @Environment(\.colorSchemeContrast) private var contrast

    init(monitor: PanelModel.Monitor, isHovered: Bool) {
        self.monitor = monitor
        self.isHovered = isHovered
        // Seed with the last picture so reopening the panel shows something on the first frame.
        _snapshot = State(initialValue: monitor.displayID.flatMap { ScreenSnapshot.cache[$0] })
    }

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 4, style: .continuous)
        Color.clear
            .overlay { fill }
            .overlay {
                if monitor.problem != nil {
                    ZStack {
                        Color.black.opacity(0.35)
                        Image(systemName: "exclamationmark.triangle.fill")
                            .font(.system(size: 13))
                            .symbolRenderingMode(.multicolor)
                    }
                }
            }
            .clipShape(shape)
            .overlay(shape.strokeBorder(.black.opacity(contrast == .increased ? 0.6 : 0.22),
                                        lineWidth: contrast == .increased ? 1 : 0.5))
            .overlay(shape.strokeBorder(Color.accentColor, lineWidth: 2).opacity(isHovered ? 1 : 0))
            .task(id: monitor.displayID) { await refreshWhileVisible() }
    }

    @ViewBuilder
    private var fill: some View {
        if monitor.displayID == nil {
            // macOS has not created this display yet, so there is nothing to show.
            Rectangle().fill(.quaternary)
        } else if let snapshot {
            Image(decorative: snapshot, scale: 1)
                .resizable()
                .interpolation(.high)
                .scaledToFill()
                .transition(.opacity)
        } else {
            LinearGradient(
                colors: [Color.accentColor.opacity(0.95), Color.accentColor.opacity(0.55)],
                startPoint: .topLeading, endPoint: .bottomTrailing
            )
        }
    }

    /// Keeps the picture roughly live while the panel is open. The task is cancelled as soon as
    /// the panel's view tree is released, so nothing is captured while it is closed.
    private func refreshWhileVisible() async {
        guard let displayID = monitor.displayID else { return }
        while !Task.isCancelled {
            if let image = await ScreenSnapshot.capture(displayID) {
                guard !Task.isCancelled else { return }
                if snapshot == nil {
                    withAnimation(.easeOut(duration: 0.2)) { snapshot = image }
                } else {
                    snapshot = image
                }
            }
            try? await Task.sleep(for: ScreenSnapshot.interval)
        }
    }
}

/// Small pictures of the dock's displays for the map, taken with the Screen Recording access
/// LinkLite already has. Reading the desktop picture file instead would need a separate grant
/// whenever the wallpaper lives in Photos or Pictures.
@MainActor
private enum ScreenSnapshot {
    static var cache: [CGDirectDisplayID: CGImage] = [:]
    static let interval: Duration = .seconds(2)
    /// Twice the widest rectangle the map draws.
    private static let maxWidth = 560

    static func capture(_ displayID: CGDirectDisplayID) async -> CGImage? {
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            guard let display = content.displays.first(where: { $0.displayID == displayID }) else { return nil }
            // Leave the panel itself out, or it would show up inside its own map.
            let ownApp = content.applications.filter { $0.processID == ProcessInfo.processInfo.processIdentifier }
            let filter = SCContentFilter(display: display, excludingApplications: ownApp, exceptingWindows: [])
            let config = SCStreamConfiguration()
            let scale = min(1, Double(maxWidth) / Double(max(display.width, 1)))
            config.width = max(1, Int(Double(display.width) * scale))
            config.height = max(1, Int(Double(display.height) * scale))
            config.showsCursor = false
            config.colorSpaceName = CGColorSpace.sRGB
            let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
            cache[displayID] = image
            return image
        } catch {
            return nil
        }
    }
}

// MARK: - Placeholders

/// A dashed outline where the first display will appear, with a slow sweep of light while
/// LinkLite looks for the dock.
private struct SearchingPlaceholder: View {
    let caption: String?
    let isSearching: Bool
    let namespace: Namespace.ID
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorSchemeContrast) private var contrast

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 5, style: .continuous)
        let dash = StrokeStyle(lineWidth: contrast == .increased ? 1.5 : 1, dash: [4, 3])
        VStack(spacing: 8) {
            ZStack {
                shape.fill(.primary.opacity(0.04))
                shape.strokeBorder(style: dash)
                    .foregroundStyle(contrast == .increased ? .secondary : .tertiary)
                Image(systemName: "cable.connector")
                    .font(.system(size: 18))
                    .foregroundStyle(.tertiary)
            }
            .overlay {
                if isSearching && !reduceMotion {
                    Shimmer()
                        .mask {
                            ZStack {
                                shape.fill(.white.opacity(0.25))
                                shape.strokeBorder(style: dash)
                            }
                        }
                        .transition(.opacity)
                }
            }
            .frame(width: 112, height: 63)
            .matchedGeometryEffect(id: "first", in: namespace)
            if let caption {
                Text(caption)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .lineLimit(3)
                    .fixedSize(horizontal: false, vertical: true)
                    .contentTransition(.opacity)
            }
        }
        .frame(maxWidth: .infinity, minHeight: ArrangementMap.height)
        .accessibilityElement(children: .combine)
    }
}

/// A band of light that crosses the outline every couple of seconds. Uses the frame clock instead
/// of a repeat-forever animation, which would leak into other layout changes.
private struct Shimmer: View {
    private static let period: TimeInterval = 2.2

    var body: some View {
        TimelineView(.animation) { context in
            GeometryReader { proxy in
                let t = context.date.timeIntervalSinceReferenceDate
                let phase = t.truncatingRemainder(dividingBy: Self.period) / Self.period
                let band = proxy.size.width * 0.6
                LinearGradient(
                    colors: [.primary.opacity(0), .primary.opacity(0.9), .primary.opacity(0)],
                    startPoint: .leading, endPoint: .trailing
                )
                .frame(width: band)
                .offset(x: -band + (proxy.size.width + band) * phase)
            }
        }
        .allowsHitTesting(false)
    }
}

private struct PermissionCallout: View {
    let action: () -> Void

    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: "lock.display")
                .font(.system(size: 28))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(.secondary)
                .padding(.bottom, 2)
                .accessibilityHidden(true)
            Text("Screen Recording Required")
                .font(.system(size: 13, weight: .semibold))
            Text("LinkLite captures only the displays it creates for your dock.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            Button("Open System Settings…", action: action)
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                .padding(.top, 4)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 6)
    }
}

// MARK: - Rows

private struct ToggleRow: View {
    let title: String
    let subtitle: String?
    let isOn: Bool
    let action: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .font(.system(size: 13))
                if let subtitle {
                    Text(subtitle)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .transition(.opacity)
                }
            }
            .accessibilityHidden(true)
            Spacer(minLength: 8)
            Toggle(title, isOn: Binding(get: { isOn }, set: { _ in action() }))
                .toggleStyle(.switch)
                .controlSize(.small)
                .labelsHidden()
                .accessibilityHint(subtitle ?? "")
        }
        .padding(.vertical, 4)
    }
}

/// An NSMenu item: the accent highlight fills the row while the pointer is over it.
private struct MenuRowButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        MenuRow(configuration: configuration)
    }

    private struct MenuRow: View {
        let configuration: ButtonStyleConfiguration
        @State private var isHovered = false
        @Environment(\.isEnabled) private var isEnabled

        var body: some View {
            let highlighted = isEnabled && (isHovered || configuration.isPressed)
            configuration.label
                .font(.system(size: 13))
                .lineLimit(1)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, PanelView.contentInset - PanelView.rowInset)
                .frame(height: 24)
                .foregroundStyle(highlighted ? AnyShapeStyle(.white) : AnyShapeStyle(.primary))
                .opacity(isEnabled ? 1 : 0.35)
                .background {
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(Color(nsColor: .selectedContentBackgroundColor))
                        .opacity(highlighted ? 1 : 0)
                }
                .contentShape(Rectangle())
                .onHover { isHovered = $0 }
                .animation(PanelMotion.hover, value: highlighted)
        }
    }
}

private struct PanelSeparator: View {
    @Environment(\.colorSchemeContrast) private var contrast
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        Rectangle()
            .fill(.primary.opacity(contrast == .increased ? 0.35 : 0.12))
            .frame(height: 1 / displayScale)
            .padding(.horizontal, PanelView.contentInset)
            .padding(.vertical, 5)
            .accessibilityHidden(true)
    }
}


// MARK: - Surfaces

private extension View {
    /// Liquid Glass on macOS 26; the translucent popover material before that, and a solid surface
    /// when Reduce Transparency is on.
    func glassPanel() -> some View {
        modifier(GlassPanel())
    }
}

private struct GlassPanel: ViewModifier {
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.colorSchemeContrast) private var contrast

    /// Matches the system menus: 16 pt on macOS 14 and 15, rounder alongside Liquid Glass.
    static var cornerRadius: CGFloat {
        if #available(macOS 26.0, *) { 18 } else { 16 }
    }

    func body(content: Content) -> some View {
        let shape = RoundedRectangle(cornerRadius: Self.cornerRadius, style: .continuous)
        let rim = shape.strokeBorder(.primary.opacity(contrast == .increased ? 0.3 : 0.08), lineWidth: 0.5)
        if reduceTransparency {
            content
                .background(Color(nsColor: .windowBackgroundColor), in: shape)
                .overlay(rim)
        } else if #available(macOS 26.0, *) {
            content
                .glassEffect(.regular, in: shape)
                .overlay { if contrast == .increased { rim } }
        } else {
            content
                .background(VisualEffectBackground().clipShape(shape))
                .overlay(rim)
        }
    }
}

private struct VisualEffectBackground: NSViewRepresentable {
    func makeNSView(context: Context) -> NSVisualEffectView {
        let view = NSVisualEffectView()
        view.material = .popover
        view.blendingMode = .behindWindow
        view.state = .active
        return view
    }

    func updateNSView(_ view: NSVisualEffectView, context: Context) {}
}

// MARK: - Previews

@MainActor
private func previewModel(
    _ status: PanelModel.Status,
    monitors: [PanelModel.Monitor] = [],
    message: String = ""
) -> PanelModel {
    let model = PanelModel()
    model.status = status
    model.monitors = monitors
    model.message = message
    model.openAtLogin = true
    model.updatesAvailable = true
    model.canCheckForUpdates = true
    return model
}

private let previewDell = PanelModel.Monitor(
    id: 0, name: "DELL U2723QE", detail: "3840 × 2160 · 60 Hz", problem: nil, displayID: nil,
    frame: CGRect(x: 0, y: 0, width: 2560, height: 1440)
)
private let previewLG = PanelModel.Monitor(
    id: 1, name: "LG HDR 4K", detail: "3840 × 2160 · 30 Hz", problem: nil, displayID: nil,
    frame: CGRect(x: 2560, y: 180, width: 1920, height: 1080)
)

#Preview("Searching") {
    PanelView(model: previewModel(.searching), presentation: PanelPresentation(phase: .shown)) { _ in }
}

#Preview("Connected, one display") {
    PanelView(model: previewModel(.connected, monitors: [previewDell]),
              presentation: PanelPresentation(phase: .shown)) { _ in }
}

#Preview("Connected, two displays") {
    PanelView(model: previewModel(.connected, monitors: [previewDell, previewLG]),
              presentation: PanelPresentation(phase: .shown)) { _ in }
}

#Preview("Problem") {
    let broken = PanelModel.Monitor(
        id: 1, name: "LG HDR 4K", detail: "3840 × 2160 · 30 Hz",
        problem: "Not enough USB bandwidth", displayID: nil, frame: nil
    )
    let dell = PanelModel.Monitor(
        id: 0, name: "DELL U2723QE", detail: "3840 × 2160 · 60 Hz", problem: nil, displayID: nil, frame: nil
    )
    return PanelView(model: previewModel(.connected, monitors: [dell, broken]),
                     presentation: PanelPresentation(phase: .shown)) { _ in }
}

#Preview("Needs permission") {
    PanelView(model: previewModel(.needsPermission), presentation: PanelPresentation(phase: .shown)) { _ in }
}
