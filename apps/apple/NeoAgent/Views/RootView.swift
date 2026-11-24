import SwiftUI
import Tailnet

struct RootView: View {
    @Environment(TailnetService.self) private var tailnet

    var body: some View {
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
