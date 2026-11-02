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

@Test func decodesAPageOfEvents() throws {
    // As served by agentd 0.15.1 (GET /api/v1/events/page).
    let json = """
        {"events":[{"seq":12,"ts":"2026-11-01T10:00:00Z","kind":"message","data":{"role":"user","text":"hi"}}],
         "more":true,"last_seq":40}
        """
    let page = try JSONDecoder().decode(EventPage.self, from: Data(json.utf8))
    #expect(page.events.map(\.seq) == [12])
    #expect(page.more && page.lastSeq == 40)
}

@Test func decodesMemoryFromV1AndV2Desktops() throws {
    let v1 = try JSONDecoder().decode(
        AgentMemory.self, from: Data(#"{"id":1,"text":"x","trusted":false,"created":"2026-11-01T10:00:00Z"}"#.utf8))
    #expect(v1.kind == nil && v1.isInferred)
    let v2 = try JSONDecoder().decode(
        AgentMemory.self,
        from: Data(
            #"{"id":2,"text":"Always cc the CFO","trusted":false,"created":"2026-11-01T10:00:00.123Z","kind":"fact","status":"active","trust":"agent_inferred","pending_reason":null,"importance":0.5,"version":1,"recall_count":0}"#
                .utf8))
    #expect(v2.kind == "fact" && v2.status == "active" && v2.isInferred)
}

@Test func decodesTheMemoryScreensData() throws {
    let memory = try JSONDecoder().decode(
        AgentMemory.self,
        from: Data(
            #"{"id":8,"text":"The standup moved to 10:00","trusted":false,"created":"2026-11-01T10:00:00.123Z","kind":"fact","status":"active","trust":"agent_inferred","version":1,"valid_until":1793750399,"source_run":"a1b2c3d4e5f6","origin":"agent","importance":0.5,"recall_count":0}"#
                .utf8))
    #expect(memory.sourceRun == "a1b2c3d4e5f6" && memory.origin == "agent" && memory.version == 1)
    #expect(memory.endDate == Date(timeIntervalSince1970: 1_793_750_399) && !memory.isGone)
    let stats = try JSONDecoder().decode(
        ConsolidationStats.self,
        from: Data(
            #"{"mode":"auto","model":"zai-org/GLM-5.3-Flash","daily_cap_usd":0.5,"month_cost":0.0123,"last_run":{"at":"2026-11-01T18:00:00.000Z","status":"done","cost":0.0006,"counts":{"saved":1,"merged":2,"expired":0,"rejected":1}},"waiting_tasks":3}"#
                .utf8))
    #expect(stats.mode == "auto" && stats.lastRun?.counts?.merged == 2 && stats.waitingTasks == 3)
    let status = try JSONDecoder().decode(
        AgentStatus.self,
        from: Data(
            #"{"desktop":"d","busy":false,"settings":{"model":"m","allow_actions":true},"last_seq":1,"known_models":["m"],"shell_tool":true,"version":"0.15.0","stage":"working","stage_detail":{"run":"t1"},"memory_v2":true}"#
                .utf8))
    #expect(status.stage == "working" && status.stageDetail?["run"]?.string == "t1" && status.memoryV2 == true)
}
