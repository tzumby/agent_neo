import AgentAPI
import ChatCore
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
                // A new Tailscale session (after the app was suspended) restarts the stream.
                connectionKey: "\(desktop.host)|\(ObjectIdentifier(session))"
            )
        } else {
            ContentUnavailableView(
                "No desktop yet",
                systemImage: "desktopcomputer",
                description: Text("No device tagged tag:cloud-desktop is on your tailnet.")
            )
        }
    }
}
