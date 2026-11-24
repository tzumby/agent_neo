import SwiftUI
import Tailnet

@main
struct NeoAgentApp: App {
    @State private var tailnet = TailnetService(hostName: "neo-agent-iphone")
    @State private var session = DesktopSession()
    @State private var grace = BackgroundGrace()
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            Group {
                #if DEBUG
                    if let preview = TakeOverPreview.fromEnvironment() {
                        preview
                    } else if let preview = ChatPreview.fromEnvironment() {
                        preview
                    } else {
                        root
                    }
                #else
                    root
                #endif
            }
            .preferredColorScheme(.dark)  // Grok-style: dark only
            .environment(PushCenter.shared)
            .environment(session)
        }
        .onChange(of: scenePhase) { old, new in
            if new == .background {
                grace.begin()
            } else if old == .background {
                // iOS takes the Tailscale node's loopback proxy away once the app
                // is suspended; then start a fresh node (same identity).
                let suspended = grace.end()
                Task { suspended ? await tailnet.restart() : await tailnet.ensureConnected() }
            }
        }
    }

    private var root: some View {
        RootView()
            .environment(tailnet)
            .task { tailnet.start() }
    }
}
