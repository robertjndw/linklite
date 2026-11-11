// swift-tools-version: 6.0
import Foundation
import PackageDescription

/// The Rust engine (`cargo build --release --manifest-path engine/Cargo.toml`) builds
/// libdllite_engine.a here, with libusb linked in statically.
let engineLibDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("engine/target/release")
    .path

let package = Package(
    name: "linklite",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "dlprobe", targets: ["dlprobe"]),
        .executable(name: "LinkLite", targets: ["LinkLite"]),
        .executable(name: "linklite-bench", targets: ["linklite-bench"])
    ],
    dependencies: [
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.10.1")
    ],
    targets: [
        .target(name: "DLProbeCore"),
        .executableTarget(
            name: "dlprobe",
            dependencies: ["DLProbeCore"]
        ),
        .target(
            name: "CDLLite",
            linkerSettings: [
                .unsafeFlags(["-L", engineLibDir]),
                .linkedLibrary("dllite_engine"),
                .linkedFramework("IOKit"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Security"),
                .linkedFramework("CoreGraphics")
            ]
        ),
        // The Metal strip encoder. A library of its own so its tests and the benchmark link it.
        .target(
            name: "LinkLiteGPU",
            dependencies: ["CDLLite"],
            linkerSettings: [
                .linkedFramework("Metal"),
                .linkedFramework("CoreVideo")
            ]
        ),
        .executableTarget(
            name: "LinkLite",
            dependencies: [
                "CDLLite",
                "LinkLiteGPU",
                .product(name: "Sparkle", package: "Sparkle")
            ],
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("ServiceManagement"),
                // build-app.sh copies Sparkle.framework into LinkLite.app/Contents/Frameworks.
                .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"])
            ]
        ),
        // Records clips for the replay benchmark and replays them through the engine's pixel path,
        // fed by the CPU or the Metal encoder (docs/BENCHMARK.md). Needs no dock.
        .executableTarget(
            name: "linklite-bench",
            dependencies: ["CDLLite", "LinkLiteGPU"],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit")
            ]
        ),
        .testTarget(
            name: "DLProbeCoreTests",
            dependencies: ["DLProbeCore"]
        ),
        .testTarget(
            name: "LinkLiteGPUTests",
            dependencies: ["LinkLiteGPU", "CDLLite"]
        )
    ]
)
