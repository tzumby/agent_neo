import SwiftUI
import Tailnet

struct RootView: View {
    @Environment(TailnetService.self) private var tailnet
    /// A setup link's key, waiting for the user's OK.
    @State private var setupKey: String?

    var body: some View {
        phases
            // A tester's setup link (clouddesktop://join?key=…), tapped or pasted.
            .onOpenURL { url in
                if let key = SetupLink.authKey(in: url) { setupKey = key }
            }
            .environment(\.useSetupLink, UseSetupLink { setupKey = $0 })
            .alert(
                "Connect to your desktop?",
                isPresented: Binding(get: { setupKey != nil }, set: { if !$0 { setupKey = nil } })
            ) {
                Button("Connect") {
                    if let key = setupKey { Task { await tailnet.join(authKey: key) } }
                    setupKey = nil
                }
                Button("Cancel", role: .cancel) { setupKey = nil }
            } message: {
                Text(joinMessage)
            }
    }

    private var joinMessage: String {
        switch tailnet.phase {
        case .running, .reconnecting:
            "This setup link connects the app to the desktop it was made for, and disconnects it from the one it's on now. Only open links sent to you by whoever runs your desktop."
        default:
            "This setup link connects the app to the desktop it was made for. Only open links sent to you by whoever runs your desktop."
        }
    }

    @ViewBuilder private var phases: some View {
        switch tailnet.phase {
        case .stopped, .starting:
            SplashView(status: "Connecting…")
        case .needsLogin(let url):
            LoginView(loginURL: url)
        case .running, .reconnecting:
            // Stays up while Tailscale reconnects, so the chat and an open
            // take-over view survive a trip to another app.
            NavigationStack { DesktopHome() }
        case .failed(let message):
            ContentUnavailableView {
                Label("Tailscale isn't connected", systemImage: "exclamationmark.triangle")
            } description: {
                Text(message)
            } actions: {
                Button("Try again") { Task { await tailnet.restart() } }
            }
        }
    }
}

/// Hands a pasted setup link's key to RootView, which asks before joining.
struct UseSetupLink {
    let use: (String) -> Void
}

extension EnvironmentValues {
    @Entry var useSetupLink: UseSetupLink? = nil
}
