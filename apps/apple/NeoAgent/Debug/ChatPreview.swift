#if DEBUG
import AgentAPI
import ChatCore
import SwiftUI

/// Debug builds only: the chat against any agentd-compatible server, without
/// Tailscale (e.g. the Simulator and a local stand-in). Launch with
/// CHAT_PREVIEW_URL=http://127.0.0.1:8899 (through simctl: SIMCTL_CHILD_CHAT_PREVIEW_URL), and
/// PREVIEW_SCREEN=memory, settings, schedules or keys for that screen.
struct ChatPreview: View {
    let url: URL
    @State private var store = ChatStore()

    static func fromEnvironment() -> ChatPreview? {
        ProcessInfo.processInfo.environment["CHAT_PREVIEW_URL"].flatMap(URL.init(string:)).map { ChatPreview(url: $0) }
    }

    private var client: AgentClient { AgentClient(baseURL: url, session: .shared) }

    var body: some View {
        NavigationStack {
            // PREVIEW_SCREEN opens another screen instead (screenshots).
            switch ProcessInfo.processInfo.environment["PREVIEW_SCREEN"] {
            case "memory":
                MemoryView(client: client)
            case "settings":
                SettingsView(store: store, client: client).task { await store.run(client: client) }
            case "schedules":
                SchedulesView(client: client)
            case "keys":
                SecretsView(client: client)
            default:
                ChatView(
                    store: store, client: client,
                    desktopName: "neo-agent", endpoint: nil, connectionKey: url.absoluteString)
            }
        }
    }
}
#endif
