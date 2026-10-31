import SwiftUI
import Tailnet

@main
struct CloudDesktopApp: App {
    @State private var tailnet = TailnetService(hostName: "cloud-desktop-iphone")
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            #if DEBUG
                if let preview = TakeOverPreview.fromEnvironment() {
                    preview
                } else {
                    root
                }
            #else
                root
            #endif
        }
        .onChange(of: scenePhase) { old, new in
            // iOS takes the Tailscale node's loopback proxy away while the app is
            // suspended; start a fresh node (same identity) when it comes back.
            if old == .background && new != .background {
                Task { await tailnet.restart() }
            }
        }
    }

    private var root: some View {
        RootView()
            .environment(tailnet)
            .task { tailnet.start() }
    }
}
