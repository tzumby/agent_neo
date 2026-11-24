import AgentAPI
import ChatCore
import DesktopViewer
import Observation
import SwiftUI
import Tailnet

/// Picks your desktop on the tailnet and shows its chat.
struct DesktopHome: View {
    @Environment(TailnetService.self) private var tailnet
    @Environment(DesktopSession.self) private var session

    var body: some View {
        if let desktop = tailnet.desktop, let client = tailnet.client(for: desktop) {
            ChatView(
                store: session.store(for: desktop.host),
                client: client,
                desktopName: desktop.hostName,
                endpoint: tailnet.endpoint(for: desktop),
                // A new Tailscale session (after the app was suspended) restarts the stream.
                connectionKey: "\(desktop.host)|\(tailnet.generation)"
            )
        } else {
            ContentUnavailableView(
                "No desktop yet",
                systemImage: "display",
                description: Text("No device tagged tag:neo-agent is on your tailnet. Deploy the desktop (make deploy), and sign in here with the Tailscale account it's on.")
            )
            .background(Color.black)
        }
    }
}

/// The app's desktop chats, shared by its windows (the Mac's take-over
/// window acts on the chat's cards too).
@MainActor
@Observable
final class DesktopSession {
    /// The Mac's take-over window.
    static let desktopWindow = "desktop"

    /// One chat per desktop: another account has other desktops. A cache,
    /// filled as views ask, so not observed.
    @ObservationIgnored private var stores: [String: ChatStore] = [:]
    /// The Action needed card the Mac's take-over window was opened for.
    var takeOverAction: String?

    func store(for host: String) -> ChatStore {
        if let store = stores[host] { return store }
        let store = ChatStore()
        stores[host] = store
        return store
    }
}

extension TailnetService {
    /// The desktop the app shows: the first one online.
    var desktop: TailnetStatus.Peer? {
        desktops.first { $0.online == true } ?? desktops.first
    }

    /// agentd on the desktop, through the tailnet.
    func client(for desktop: TailnetStatus.Peer) -> AgentClient? {
        session.map { AgentClient(host: desktop.host, session: $0) }
    }

    /// VNC through the node's proxy, to the desktop's tailnet IP (no DNS needed).
    func endpoint(for desktop: TailnetStatus.Peer) -> DesktopEndpoint? {
        guard let proxy else { return nil }
        return DesktopEndpoint(
            proxy: SOCKSProxy(host: proxy.host, port: proxy.port, username: proxy.username, password: proxy.password),
            host: desktop.ipv4 ?? desktop.host)
    }
}
