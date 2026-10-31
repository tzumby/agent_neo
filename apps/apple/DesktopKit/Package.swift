// swift-tools-version: 6.0
// Shared code for the iPhone and Mac apps: the in-app Tailscale node, the
// client for agentd's API, and the chat built from agentd's events.

import PackageDescription

let package = Package(
    name: "DesktopKit",
    platforms: [.iOS("18.1"), .macOS("15.6")],
    products: [
        .library(name: "Tailnet", targets: ["Tailnet"]),
        .library(name: "AgentAPI", targets: ["AgentAPI"]),
        .library(name: "ChatCore", targets: ["ChatCore"]),
    ],
    targets: [
        // Built by scripts/build-tailscalekit.sh (not checked in).
        .binaryTarget(name: "TailscaleKit", path: "Vendor/TailscaleKit.xcframework"),
        .target(name: "Tailnet", dependencies: ["TailscaleKit"]),
        .target(name: "AgentAPI"),
        .target(name: "ChatCore", dependencies: ["AgentAPI"]),
        .testTarget(name: "DesktopKitTests", dependencies: ["Tailnet", "AgentAPI", "ChatCore"]),
    ]
)
