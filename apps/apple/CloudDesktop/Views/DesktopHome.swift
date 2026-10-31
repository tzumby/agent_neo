import AgentAPI
import ChatCore
import DesktopViewer
import SwiftUI
import Tailnet

/// Picks your desktop on the tailnet and shows its chat.
struct DesktopHome: View {
    @Environment(TailnetService.self) private var tailnet
    @State private var store = ChatStore()

    private var desktop: TailnetStatus.Peer? {
        tailnet.desktops.first { $0.online == true } ?? tailnet.desktops.first
    }

    var body: some View {
        if let desktop, let session = tailnet.session {
            ChatView(
                store: store,
                client: AgentClient(host: desktop.host, session: session),
                desktopName: desktop.hostName,
                endpoint: endpoint(for: desktop),
                // A new Tailscale session (after the app was suspended) restarts the stream.
                connectionKey: "\(desktop.host)|\(tailnet.generation)"
            )
        } else if !tailnet.peersLoaded {
            SplashView(status: "Finding your desktop…")
        } else {
            ContentUnavailableView(
                "No desktop yet",
                systemImage: "display",
                description: Text("No device tagged tag:cloud-desktop is on your tailnet.")
            )
            .background(Color.black)
        }
    }

    /// VNC through the node's proxy, to the desktop's tailnet IP (no DNS needed).
    private func endpoint(for desktop: TailnetStatus.Peer) -> DesktopEndpoint? {
        guard let proxy = tailnet.proxy else { return nil }
        return DesktopEndpoint(
            proxy: SOCKSProxy(host: proxy.host, port: proxy.port, username: proxy.username, password: proxy.password),
            host: desktop.ipv4 ?? desktop.host)
    }
}
