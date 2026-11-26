import AgentAPI
import ChatCore
import DesktopViewer
import Observation
import SwiftUI
import Tailnet

/// Picks your desktop on the tailnet and shows its conversations: the
/// iPhone's list and chat (PhoneHome), the Mac's sidebar and chat (MacHome).
struct DesktopHome: View {
    @Environment(TailnetService.self) private var tailnet
    @Environment(DesktopSession.self) private var session

    var body: some View {
        if let desktop = tailnet.desktop, let client = tailnet.client(for: desktop) {
            let home = DesktopHomeContent(
                store: session.store(for: desktop.host),
                client: client,
                desktopName: desktop.hostName,
                endpoint: tailnet.endpoint(for: desktop),
                // A new Tailscale session (after the app was suspended) restarts the stream.
                connectionKey: "\(desktop.host)|\(tailnet.generation)"
            )
            #if os(iOS)
                PhoneHome(home: home)
            #else
                MacHome(home: home)
            #endif
        } else {
            ContentUnavailableView(
                "No desktop yet",
                systemImage: "display",
                description: Text("No device tagged tag:neo-agent is on your tailnet. Deploy the desktop (make deploy), and sign in here with the Tailscale account it's on.")
            )
            .background(Theme.background)
        }
    }
}

/// What a home shows: a desktop's store and how to reach it.
struct DesktopHomeContent {
    let store: ChatStore
    let client: AgentClient
    let desktopName: String
    let endpoint: DesktopEndpoint?
    let connectionKey: String

    /// A conversation's chat.
    @MainActor func chat(_ id: String) -> ChatView {
        ChatView(
            store: store, conversation: store.conversation(id), client: client, desktopName: desktopName,
            endpoint: endpoint, connectionKey: connectionKey)
    }
}

/// What showing a desktop does, whatever the layout: keeps its store in sync
/// (one event stream for all its conversations), registers this device for
/// notifications, shows the store's notices, and covers everything with the
/// splash until the first chat is in.
struct DesktopSync: ViewModifier {
    let home: DesktopHomeContent
    @Environment(PushCenter.self) private var push

    private var store: ChatStore { home.store }

    func body(content: Content) -> some View {
        content
            // Until the newest page is in, the splash covers everything (bars included).
            // Only the splash fades: the chat under it is already at its end.
            .overlay {
                ZStack {
                    if !store.ready {
                        SplashView(status: "Connecting to \(home.desktopName)…", detail: offlineDetail)
                            .transition(.opacity)
                    }
                }
                .animation(.easeOut(duration: 0.25), value: store.ready)
            }
            .task(id: home.connectionKey) { await store.run(client: home.client) }
            .task(id: store.connection == .live) {
                if store.connection == .live { await push.register() }
            }
            .task(id: "\(push.deviceToken ?? "")|\(home.connectionKey)") { await registerForPush() }
            .alert(
                "Agent Neo",
                isPresented: Binding(get: { store.notice != nil }, set: { if !$0 { store.notice = nil } })
            ) {
                Button("OK") {}
            } message: {
                Text(store.notice ?? "")
            }
    }

    private var offlineDetail: String? {
        if case .offline(let reason) = store.connection { return "Still trying: \(reason)" }
        return nil
    }

    /// Hands this device's push token to the desktop, which registers it for
    /// APNs notifications. Retries while the connection comes up. Nothing
    /// happens on a desktop with no APNs key (it answers 400).
    private func registerForPush() async {
        guard push.deviceToken != nil else { return }
        let topic = Bundle.main.bundleIdentifier ?? ""
        for _ in 0..<10 {
            if store.connection == .live {
                do {
                    try await push.registerDevice(on: home.client, topic: topic)
                    return
                } catch let error as AgentError where error.statusCode == 400 {
                    return  // notifications to the apps aren't set up on this desktop
                } catch {}
            }
            try? await Task.sleep(for: .seconds(3))
            if Task.isCancelled { return }
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

    /// One store per desktop: another account has other desktops. A cache,
    /// filled as views ask, so not observed.
    @ObservationIgnored private var stores: [String: ChatStore] = [:]
    /// The Action needed card the Mac's take-over window was opened for.
    var takeOverAction: String?

    func store(for host: String) -> ChatStore {
        if let store = stores[host] { return store }
        // The conversation used last is remembered per desktop, on this device.
        let store = ChatStore(remembering: "lastConversation.\(host)")
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
