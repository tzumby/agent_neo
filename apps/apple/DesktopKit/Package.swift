// swift-tools-version: 6.0
// Shared code for the iPhone and Mac apps: the in-app Tailscale node, the
// client for agentd's API, the chat built from agentd's events, and the
// connection behind the take-over view (VNC over the tailnet).

import PackageDescription

let package = Package(
    name: "DesktopKit",
    platforms: [.iOS("18.1"), .macOS("15.6")],
    products: [
        .library(name: "Tailnet", targets: ["Tailnet"]),
        .library(name: "AgentAPI", targets: ["AgentAPI"]),
        .library(name: "ChatCore", targets: ["ChatCore"]),
        .library(name: "DesktopViewer", targets: ["DesktopViewer"]),
    ],
    dependencies: [
        // VNC (RFB) client: Tight/ZRLE, platform-neutral core, pluggable transport. MIT.
        .package(url: "https://github.com/loomhand/swift-rfb", exact: "0.1.0"),
    ],
    targets: [
        // Built by scripts/build-tailscalekit.sh (not checked in).
        .binaryTarget(name: "TailscaleKit", path: "Vendor/TailscaleKit.xcframework"),
        .target(name: "Tailnet", dependencies: ["TailscaleKit"]),
        .target(name: "AgentAPI"),
        .target(name: "ChatCore", dependencies: ["AgentAPI"]),
        .target(name: "DesktopViewer", dependencies: [.product(name: "RFB", package: "swift-rfb")]),
        .testTarget(
            name: "DesktopKitTests",
            dependencies: ["Tailnet", "AgentAPI", "ChatCore", "DesktopViewer", .product(name: "RFB", package: "swift-rfb")]
        ),
    ]
)
