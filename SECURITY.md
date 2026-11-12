# Security Policy

## Reporting a vulnerability

Please report security issues **privately** rather than opening a public issue.

Use GitHub's private vulnerability reporting on this repository: go to the **Security** tab › **Report a vulnerability**. That opens a private advisory visible only to the maintainers.

Please include enough detail to reproduce: LinkLite version, macOS version, Mac model, dock model, and steps. We'll acknowledge your report and keep you updated on a fix.

## Scope

LinkLite is a local menu-bar app. It talks to the USB dock it drives and, for updates only, to GitHub. Sparkle authenticates downloaded updates before extraction. Official releases carry EdDSA and Apple Developer ID signatures; Sparkle also supports same-Developer-ID validation when rotating an update-signing key. It has no backend or telemetry. See [PRIVACY.md](PRIVACY.md).

Areas of particular interest:

- **Screen capture.** LinkLite holds the Screen Recording permission and uses ScreenCaptureKit to read the virtual displays it creates for the dock's monitors. Any path that captures another display or window, or sends captured pixels anywhere other than the dock, is in scope.
- **Private display API.** Virtual displays are created with CoreGraphics' private `CGVirtualDisplay` classes, declared locally in `Sources/CDLLite/`. Misuse that could leave macOS with stray displays or an unusable display arrangement is in scope.
- **USB and protocol handling.** The engine (`engine/`) and the vendored Vino core (`vendor/dlcore/`) parse data from the dock, including monitor EDIDs, and run the HDCP 2.2 authentication and encrypted control session. Memory-safety or parsing bugs reachable from a malicious or malfunctioning USB device are in scope.
- **Release integrity.** Release archives on GitHub are signed with the maintainer's Developer ID, notarized by Apple, and published with a SHA-256 checksum. Report anything that lets a modified build pass as an official one.

Out of scope: the dock's own firmware, and the DisplayLink protocol design itself. Issues in upstream Vino that are not specific to LinkLite are best reported to [Vino](https://github.com/cameron1729/Vino) as well.

## Trust boundaries

Connect only docks you trust to receive the pixels displayed on their monitors. The current HDCP handshake checks possession of the receiver's supplied key, but does not validate a receiver certificate against a trusted root. It is not a guarantee of the dock's manufacturer or identity. USB replies and monitor descriptors must still be treated as untrusted input.

LinkLite runs without App Sandbox. Release builds enable hardened runtime without JIT or library-validation exceptions. Memory-safety defects therefore matter even when they require a physically attached device. The native libusb dependency includes documented descriptor-parser backports in [`vendor/libusb1-sys/README.md`](vendor/libusb1-sys/README.md); auditing Cargo package versions alone does not cover that C code.

## Supported versions

Only the latest public release is supported for security fixes.
