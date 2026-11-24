#if DEBUG
import AgentAPI
import ChatCore
import SwiftUI

/// Debug builds only: the app against any agentd-compatible server, without
/// Tailscale (e.g. the Simulator and a local stand-in). Launch with
/// CHAT_PREVIEW_URL=http://127.0.0.1:8899 (through simctl: SIMCTL_CHILD_CHAT_PREVIEW_URL),
/// PREVIEW_SESSION=<id> to open that conversation, and PREVIEW_SCREEN=
/// conversations, archived, usage (usage-end: its lower half; usage-budget:
/// its budget section, PREVIEW_WIDTH points wide), memory, settings,
/// schedules or keys for that screen, PREVIEW_DRAFT=<text> to type in the
/// composer (screenshots).
struct ChatPreview: View {
    let url: URL
    @State private var store = ChatStore(remembering: ChatPreview.remembered)

    private static let remembered = "preview.lastConversation"

    static func fromEnvironment() -> ChatPreview? {
        let environment = ProcessInfo.processInfo.environment
        guard let url = environment["CHAT_PREVIEW_URL"].flatMap(URL.init(string:)) else { return nil }
        if let session = environment["PREVIEW_SESSION"] {
            UserDefaults.standard.set(session, forKey: remembered)
        }
        return ChatPreview(url: url)
    }

    private var client: AgentClient { AgentClient(baseURL: url, session: .shared) }

    /// A size in points from the environment, if set.
    private static func size(_ name: String) -> CGFloat? {
        ProcessInfo.processInfo.environment[name].flatMap(Double.init).map { CGFloat($0) }
    }

    private var home: DesktopHomeContent {
        DesktopHomeContent(
            store: store, client: client, desktopName: "neo-agent", endpoint: nil, connectionKey: url.absoluteString)
    }

    var body: some View {
        screen.onAppear {
            if let draft = ProcessInfo.processInfo.environment["PREVIEW_DRAFT"] { store.current.draft = draft }
        }
    }

    @ViewBuilder private var screen: some View {
        switch ProcessInfo.processInfo.environment["PREVIEW_SCREEN"] {
        case "memory":
            NavigationStack { MemoryView(client: client) }
        case "settings":
            NavigationStack { SettingsView(store: store, client: client) }
                .task { await store.run(client: client) }
        case "schedules":
            NavigationStack { SchedulesView(client: client) }
        case "keys":
            NavigationStack { SecretsView(client: client) }
        case "usage":
            NavigationStack { UsageView(store: store, client: client) }
                .task { await store.run(client: client) }
        case "usage-end":
            // Its lower sections: laid out tall, seen from the bottom (the
            // iPhone's; the Mac's grouped form doesn't draw in a scroll view).
            ScrollView {
                NavigationStack { UsageView(store: store, client: client).scrollDisabled(true) }
                    .frame(height: 2300)
            }
            .defaultScrollAnchor(.bottom)
            .task { await store.run(client: client) }
        case "usage-budget":
            // The budget section alone, PREVIEW_WIDTH points wide (a Mac
            // sheet's width: where its row has to fit).
            NavigationStack { UsageBudgetPreview(store: store, client: client) }
                .frame(width: Self.size("PREVIEW_WIDTH"))
        case let screen:
            #if os(iOS)
                PhoneHome(
                    home: home,
                    initialPath: screen == "conversations" ? [] : screen == "archived" ? [.archived] : nil)
            #else
                MacHome(home: home)
            #endif
        }
    }
}
#endif
