import Foundation
import Testing

@testable import AgentAPI
@testable import Tailnet

@Test func decodesTailscaleStatus() throws {
    let json = """
        {
          "BackendState": "Running",
          "AuthURL": "",
          "Self": {"ID": "n1", "HostName": "cloud-desktop-iphone", "DNSName": "cloud-desktop-iphone.tail57aeb.ts.net.", "UserID": 42, "Online": true},
          "CurrentTailnet": {"Name": "tzumby@gmail.com", "MagicDNSSuffix": "tail57aeb.ts.net"},
          "User": {"42": {"LoginName": "tzumby@gmail.com", "DisplayName": "Raz"}},
          "Peer": {
            "nodekey:a": {"ID": "n2", "HostName": "cloud-desktop-dev", "DNSName": "cloud-desktop-dev.tail57aeb.ts.net.", "Online": true, "Tags": ["tag:cloud-desktop"], "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"]},
            "nodekey:b": {"ID": "n3", "HostName": "iphone-15-plus", "DNSName": "iphone-15-plus.tail57aeb.ts.net.", "Online": true}
          }
        }
        """
    let status = try JSONDecoder().decode(TailnetStatus.self, from: Data(json.utf8))
    #expect(status.backendState == "Running")
    #expect(status.loginName == "tzumby@gmail.com")
    #expect(status.desktops.map(\.host) == ["cloud-desktop-dev.tail57aeb.ts.net"])
    #expect(status.currentTailnet?.magicDNSSuffix == "tail57aeb.ts.net")
    #expect(status.desktops.first?.ipv4 == "100.101.102.103")
}

@Test func decodesNeedsLogin() throws {
    let json = #"{"BackendState": "NeedsLogin", "AuthURL": "https://login.tailscale.com/a/abc"}"#
    let status = try JSONDecoder().decode(TailnetStatus.self, from: Data(json.utf8))
    #expect(status.authURL == "https://login.tailscale.com/a/abc")
    #expect(status.desktops.isEmpty)
}

@Test func decodesAgentStatus() throws {
    // As served by agentd 0.1.0.
    let json = """
        {"busy":false,"desktop":"cloud-desktop-dev","known_models":["zai-org/GLM-5.3"],"last_seq":10,
         "settings":{"allow_actions":false,"model":"zai-org/GLM-5.3"},"shell_tool":true,"version":"0.1.0"}
        """
    let status = try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
    #expect(status.desktop == "cloud-desktop-dev")
    #expect(status.settings.model == "zai-org/GLM-5.3")
    #expect(status.lastSeq == 10)
}
