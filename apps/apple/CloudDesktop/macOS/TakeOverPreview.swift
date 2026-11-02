#if DEBUG
import AgentAPI
import ChatCore
import DesktopViewer
import Foundation
import SwiftUI

/// Debug builds only: the take-over view on any VNC server, without
/// Tailscale. Launch with TAKEOVER_VNC=host:port, and
/// TAKEOVER_SOCKS=host:port:user:password to go through a SOCKS5 proxy.
struct TakeOverPreview: View {
    let endpoint: DesktopEndpoint
    @State private var store = ChatStore()

    static func fromEnvironment() -> TakeOverPreview? {
        let env = ProcessInfo.processInfo.environment
        guard let vnc = env["TAKEOVER_VNC"]?.split(separator: ":").map(String.init),
            vnc.count == 2, let vncPort = UInt16(vnc[1])
        else { return nil }
        var proxy: SOCKSProxy?
        if let socks = env["TAKEOVER_SOCKS"]?.split(separator: ":", maxSplits: 3).map(String.init),
            socks.count == 4, let proxyPort = UInt16(socks[1])
        {
            proxy = SOCKSProxy(host: socks[0], port: proxyPort, username: socks[2], password: socks[3])
        }
        return TakeOverPreview(endpoint: DesktopEndpoint(proxy: proxy, host: vnc[0], port: vncPort))
    }

    var body: some View {
        MacTakeOverView(
            endpoint: endpoint, actionID: nil, store: store,
            client: AgentClient(baseURL: URL(string: "http://127.0.0.1:9")!, session: .shared))
    }
}
#endif
