import SwiftUI
import Tailnet

struct RootView: View {
    @Environment(TailnetService.self) private var tailnet

    var body: some View {
        switch tailnet.phase {
        case .stopped, .starting:
            ProgressView("Connecting to Tailscale…")
        case .needsLogin(let url):
            LoginView(loginURL: url)
        case .running:
            NavigationStack { StatusView() }
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
