// SPDX-License-Identifier: GPL-2.0-or-later

import AppKit
import CDLLite
import os
import ServiceManagement

private let log = Logger(subsystem: "com.ball.linklite", category: "app")

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let host = DisplayHost()
    private let updates = UpdateManager()
    private var statusItem: NSStatusItem?
    private var icon: StatusItemIcon?
    private let model = PanelModel()
    private lazy var panel = StatusPanelController(model: model)
    private var workspaceObservers: [NSObjectProtocol] = []

    func applicationDidFinishLaunching(_ notification: Notification) {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        icon = StatusItemIcon(statusItem: item)
        item.button?.target = self
        item.button?.action = #selector(togglePanel)
        statusItem = item

        model.toggleOpenAtLogin = { [weak self] in self?.toggleOpenAtLogin() }
        model.toggleAutomaticUpdates = { [weak self] in self?.toggleAutomaticUpdates() }
        model.checkForUpdates = { [weak self] in self?.checkForUpdates() }
        model.openScreenRecordingSettings = { [weak self] in self?.openScreenRecordingSettings() }
        model.openDisplaysSettings = { [weak self] in self?.openDisplaysSettings() }
        model.quit = { [weak self] in self?.quit() }
        panel.onShow = { [weak self] in
            self?.host.refreshScreenRecordingAccess()
            self?.refreshPanel()
        }

        observeSleepAndWake()
        host.onChange = { [weak self] in self?.refreshPanel() }
        host.start()
        refreshPanel()
    }

    func applicationWillTerminate(_ notification: Notification) {
        host.stop()
    }

    // MARK: Sleep and wake

    private func observeSleepAndWake() {
        let center = NSWorkspace.shared.notificationCenter
        let power: [(Notification.Name, Bool)] = [
            (NSWorkspace.screensDidSleepNotification, false),
            (NSWorkspace.willSleepNotification, false),
            (NSWorkspace.screensDidWakeNotification, true),
            (NSWorkspace.didWakeNotification, true)
        ]
        for (name, on) in power {
            workspaceObservers.append(center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    log.info("\(name.rawValue, privacy: .public): display power \(on ? "on" : "off")")
                    self?.host.setDisplayPower(on: on)
                    if on { self?.host.resumeCapture() }
                }
            })
        }
        // Locking the screen stops every ScreenCaptureKit stream; restart them as soon as the
        // user is back, rather than waiting for the next scheduled retry.
        workspaceObservers.append(center.addObserver(
            forName: NSWorkspace.sessionDidBecomeActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.host.resumeCapture() }
        })
        workspaceObservers.append(DistributedNotificationCenter.default().addObserver(
            forName: Notification.Name("com.apple.screenIsUnlocked"), object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                log.info("Screen unlocked")
                self?.host.resumeCapture()
            }
        })
    }

    // MARK: Panel

    @objc private func togglePanel() {
        guard let button = statusItem?.button else { return }
        panel.toggle(from: button)
    }

    private func refreshPanel() {
        if host.screenRecordingDenied {
            model.status = .needsPermission
        } else {
            switch host.engineState {
            case DLL_STATE_ENGAGED: model.status = .connected
            case DLL_STATE_RETRYING: model.status = .retrying
            case DLL_STATE_SEARCHING: model.status = .searching
            default: model.status = .starting
            }
        }
        model.message = host.engineMessage
        model.monitors = host.heads.values
            .sorted { $0.monitor.head < $1.monitor.head }
            .map { head in
                let monitor = head.monitor
                let displayID = head.display?.displayID
                return PanelModel.Monitor(
                    id: monitor.head,
                    name: monitor.name,
                    detail: "\(monitor.width) × \(monitor.height) · \(monitor.effectiveRefreshHz) Hz",
                    problem: head.problem,
                    displayID: displayID,
                    frame: displayID.map { CGDisplayBounds($0) }
                )
            }
        icon?.update(status: model.status, monitorCount: model.monitors.count,
                     hasProblem: model.monitors.contains { $0.problem != nil })
        let login = SMAppService.mainApp.status
        model.openAtLogin = login == .enabled
        model.loginNeedsApproval = login == .requiresApproval
        model.updatesAvailable = updates.isAvailable
        model.automaticUpdates = updates.automaticallyDownloadsUpdates
        model.canCheckForUpdates = updates.canCheckForUpdates
    }

    private func openScreenRecordingSettings() {
        panel.hide()
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture") {
            NSWorkspace.shared.open(url)
        }
    }

    private func openDisplaysSettings() {
        panel.hide()
        if let url = URL(string: "x-apple.systempreferences:com.apple.Displays-Settings.extension") {
            NSWorkspace.shared.open(url)
        }
    }

    private func toggleOpenAtLogin() {
        let service = SMAppService.mainApp
        do {
            switch service.status {
            case .enabled:
                try service.unregister()
            case .requiresApproval:
                SMAppService.openSystemSettingsLoginItems()
            default:
                try service.register()
            }
        } catch {
            log.error("Open at Login: \(error.localizedDescription, privacy: .public)")
            let alert = NSAlert(error: error)
            alert.messageText = "Could not change Open at Login"
            alert.runModal()
        }
        refreshPanel()
    }

    private func checkForUpdates() {
        panel.hide()
        updates.checkForUpdates()
    }

    private func toggleAutomaticUpdates() {
        updates.setAutomaticallyDownloadsUpdates(!updates.automaticallyDownloadsUpdates)
        refreshPanel()
    }

    private func quit() {
        host.stop()
        NSApp.terminate(nil)
    }
}
