#if DEBUG
import AgentAPI
import ChatCore
import DesktopViewer
import Foundation
import SwiftUI

/// Debug builds only: opens the take-over screen on a desktop reached through
/// any SOCKS5 proxy, without Tailscale (e.g. the Simulator with an SSM port
/// forward and a local proxy). Launch with
///   TAKEOVER_SOCKS=host:port:user:password TAKEOVER_VNC=host:port [TAKEOVER_KEYBOARD=1]
/// (through simctl: prefix each with SIMCTL_CHILD_).
struct TakeOverPreview: View {
    let endpoint: DesktopEndpoint
    let keyboard: Bool
    @State private var store = ChatStore()

    static func fromEnvironment() -> TakeOverPreview? {
        let env = ProcessInfo.processInfo.environment
        guard let socks = env["TAKEOVER_SOCKS"]?.split(separator: ":", maxSplits: 3).map(String.init),
            socks.count == 4, let proxyPort = UInt16(socks[1]),
            let vnc = env["TAKEOVER_VNC"]?.split(separator: ":").map(String.init),
            vnc.count == 2, let vncPort = UInt16(vnc[1])
        else { return nil }
        let proxy = SOCKSProxy(host: socks[0], port: proxyPort, username: socks[2], password: socks[3])
        return TakeOverPreview(
            endpoint: DesktopEndpoint(proxy: proxy, host: vnc[0], port: vncPort),
            keyboard: env["TAKEOVER_KEYBOARD"] == "1")
    }

    var body: some View {
        TakeOverView(
            endpoint: endpoint, actionID: nil, store: store,
            client: AgentClient(baseURL: URL(string: "http://127.0.0.1:9")!, session: .shared),
            opensKeyboard: keyboard)
    }
}
#endif
