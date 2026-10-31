#if DEBUG
import AgentAPI
import ChatCore
import SwiftUI

/// Debug builds only: the chat against any agentd-compatible server, without
/// Tailscale (e.g. the Simulator and a local stand-in). Launch with
/// CHAT_PREVIEW_URL=http://127.0.0.1:8899 (through simctl: SIMCTL_CHILD_CHAT_PREVIEW_URL).
struct ChatPreview: View {
    let url: URL
    @State private var store = ChatStore()

    static func fromEnvironment() -> ChatPreview? {
        ProcessInfo.processInfo.environment["CHAT_PREVIEW_URL"].flatMap(URL.init(string:)).map { ChatPreview(url: $0) }
    }

    var body: some View {
        NavigationStack {
            ChatView(
                store: store, client: AgentClient(baseURL: url, session: .shared),
                desktopName: "cloud-desktop-dev", endpoint: nil, connectionKey: url.absoluteString)
        }
    }
}
#endif
