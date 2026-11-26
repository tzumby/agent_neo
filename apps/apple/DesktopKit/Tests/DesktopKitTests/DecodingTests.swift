import Foundation
import Testing

@testable import AgentAPI
@testable import Tailnet

@Test func decodesTailscaleStatus() throws {
    let json = """
        {
          "BackendState": "Running",
          "AuthURL": "",
          "Self": {"ID": "n1", "HostName": "neo-agent-iphone", "DNSName": "neo-agent-iphone.tail57aeb.ts.net.", "UserID": 42, "Online": true},
          "CurrentTailnet": {"Name": "me@example.com", "MagicDNSSuffix": "tail57aeb.ts.net"},
          "User": {"42": {"LoginName": "me@example.com", "DisplayName": "Me"}},
          "Peer": {
            "nodekey:a": {"ID": "n2", "HostName": "neo-agent", "DNSName": "neo-agent.tail57aeb.ts.net.", "Online": true, "Tags": ["tag:neo-agent"], "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0::1"]},
            "nodekey:b": {"ID": "n3", "HostName": "iphone-15-plus", "DNSName": "iphone-15-plus.tail57aeb.ts.net.", "Online": true}
          }
        }
        """
    let status = try JSONDecoder().decode(TailnetStatus.self, from: Data(json.utf8))
    #expect(status.backendState == "Running")
    #expect(status.loginName == "me@example.com")
    #expect(status.desktops.map(\.host) == ["neo-agent.tail57aeb.ts.net"])
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
        {"busy":false,"desktop":"neo-agent","known_models":["zai-org/GLM-5.3"],"last_seq":10,
         "settings":{"allow_actions":false,"model":"zai-org/GLM-5.3"},"shell_tool":true,"version":"0.1.0"}
        """
    let status = try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
    #expect(status.desktop == "neo-agent")
    #expect(status.settings.model == "zai-org/GLM-5.3")
    #expect(status.lastSeq == 10)
    #expect(status.settings.autoApprove == false)
}

@Test func anAnswerSendsTextOnlyWhenEdited() throws {
    let plain = try JSONEncoder().encode(ApprovalAnswer(approved: true, text: nil))
    #expect(String(decoding: plain, as: UTF8.self) == #"{"approved":true}"#)
    let edited = try JSONSerialization.jsonObject(
        with: JSONEncoder().encode(ApprovalAnswer(approved: true, text: "Hi Lawrence"))) as? [String: Any]
    #expect(edited?["text"] as? String == "Hi Lawrence")
}

@Test func autoApproveRoundTrips() throws {
    let json = #"{"model":"m","allow_actions":true,"auto_approve":true,"auto_approve_version":"v1"}"#
    var settings = try JSONDecoder().decode(AgentSettings.self, from: Data(json.utf8))
    #expect(settings.autoApprove)
    settings.autoApprove = false
    let sent = try JSONDecoder().decode(AgentSettings.self, from: JSONEncoder().encode(settings))
    #expect(sent.autoApprove == false)
    #expect(sent.autoApproveVersion == "v1")
}

@Test func pixelInputIsOffWithoutIt() throws {
    // An older desktop: no Pixel input, so no toggle.
    let old = try JSONDecoder().decode(AgentSettings.self, from: Data(#"{"model":"m","allow_actions":true}"#.utf8))
    #expect(!old.pixelInput && !old.pixelInputAvailable)
    let json = #"{"model":"m","allow_actions":true,"pixel_input":true,"pixel_input_available":true}"#
    let settings = try JSONDecoder().decode(AgentSettings.self, from: Data(json.utf8))
    #expect(settings.pixelInput && settings.pixelInputAvailable)
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
    #expect(v1.kind == nil && v1.needsConfirmation)
    let v2 = try JSONDecoder().decode(
        AgentMemory.self,
        from: Data(
            #"{"id":2,"text":"Always cc the CFO","trusted":false,"created":"2026-11-01T10:00:00.123Z","kind":"fact","status":"active","trust":"agent_inferred","pending_reason":null,"importance":0.5,"version":1,"recall_count":0}"#
                .utf8))
    #expect(v2.kind == "fact" && v2.status == "active" && v2.needsConfirmation)
}

@Test func importedMemoriesWaitForConfirmationLikeTheAgents() throws {
    let memory = { (trust: String) in
        try JSONDecoder().decode(
            AgentMemory.self,
            from: Data(
                #"{"id":3,"text":"Prefers short replies","trusted":true,"created":"2026-11-01T10:00:00Z","kind":"preference","status":"active","trust":"\#(trust)","version":1}"#
                    .utf8))
    }
    #expect(try memory("imported").needsConfirmation)
    #expect(try memory("agent_inferred").needsConfirmation)
    #expect(try !memory("user_stated").needsConfirmation)
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

// MARK: Conversations and usage (agentd 0.16.0)

@Test func decodesAConversation() throws {
    // As in docs/conversations-and-usage.md §3.1.
    let json = """
        {"sessions":[{"id":"main","title":"Plan the trip to Lisbon","created":1795530000,"updated":1795532478,
          "archived":false,"state":"waiting","needs_you":"secret",
          "queued":[{"id":"q1a2b3c4d5e6","text":"Book the hotel","by":"iPhone","created":1795532470}],
          "last":{"role":"assistant","text":"first 160 characters…","ts":1795532478}},
         {"id":"s2","title":"","created":1795532500,"updated":1795532500,"archived":false,"state":"thinking",
          "needs_you":null,"queued":[],"last":null}],
         "running_limit":3}
        """
    let list = try JSONDecoder().decode(SessionList.self, from: Data(json.utf8))
    #expect(list.runningLimit == 3)
    let main = try #require(list.sessions.first)
    #expect(main.id == AgentSession.main && main.displayTitle == "Plan the trip to Lisbon")
    #expect(main.state == .waiting && main.needsYou == "secret" && !main.archived)
    #expect(main.updated == Date(timeIntervalSince1970: 1_795_532_478))
    #expect(main.queued == [QueuedMessage(id: "q1a2b3c4d5e6", text: "Book the hotel", by: "iPhone", created: Date(timeIntervalSince1970: 1_795_532_470))])
    #expect(main.last?.role == "assistant" && main.last?.ts == Date(timeIntervalSince1970: 1_795_532_478))
    let blank = list.sessions[1]
    #expect(blank.displayTitle == "New conversation" && blank.last == nil)
    #expect(blank.state == .idle, "a state this app doesn't know reads as idle")
    #expect(blank.isBlank && !main.isBlank)
}

@Test func aSendStartsATaskOrQueues() throws {
    let decode = { (json: String) in try JSONDecoder().decode(Sent.self, from: Data(json.utf8)) }
    #expect(try decode(#"{"task":"a1b2c3"}"#) == .started(task: "a1b2c3"))
    #expect(try decode(#"{"queued":{"id":"q1","position":2}}"#) == .queued(id: "q1", position: 2))
}

@Test func decodesTheStatusOfADesktopWithConversations() throws {
    let json = """
        {"desktop":"d","busy":true,"settings":{"model":"m","allow_actions":true},"last_seq":1,"known_models":["m"],
         "shell_tool":true,"version":"0.16.0","stage":"needs_you/approval",
         "stage_detail":{"card":"a1","kind":"browser","session":"s2"},
         "sessions":{"working":2,"waiting":1,"queued":3,"running_limit":3},
         "budget":{"monthly_usd":50.0,"spent":41.20,"fraction":0.824,"paused":false,"month":"2026-11"}}
        """
    let status = try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
    #expect(status.sessions?.working == 2 && status.sessions?.queued == 3 && status.sessions?.runningLimit == 3)
    #expect(status.budget == Budget(monthlyUsd: 50, spent: 41.2, fraction: 0.824, paused: false, month: "2026-11"))
    #expect(status.stageDetail?["session"]?.string == "s2")
    // Without a budget set.
    let none = try JSONDecoder().decode(
        Budget.self, from: Data(#"{"monthly_usd":null,"spent":3.5,"fraction":null,"paused":false,"month":"2026-11"}"#.utf8))
    #expect(none.monthlyUsd == nil && none.fraction == nil && none.spent == 3.5)
    // An older desktop has neither.
    let old = try JSONDecoder().decode(
        AgentStatus.self,
        from: Data(#"{"desktop":"d","busy":false,"settings":{"model":"m","allow_actions":true},"last_seq":1,"known_models":[],"shell_tool":true,"version":"0.15.2"}"#.utf8))
    #expect(old.sessions == nil && old.budget == nil)
}

@Test func decodesUsage() throws {
    // As in docs/conversations-and-usage.md §3.5.
    let json = """
        {"currency":"USD","estimated":true,"timezone":"America/Toronto","month":"2026-11",
         "today":1.23,"this_month":41.20,"last_month":38.10,"all_time":210.55,
         "daily":[{"date":"2026-10-26","cost":0.42,"runs":7},{"date":"2026-10-27","cost":0,"runs":0}],
         "by_kind":{"chat":30.10,"scheduled":9.80,"tidy_up":1.30},
         "by_model":[{"model":"zai-org/GLM-5.3","cost":35.0,"runs":120,"input_tokens":1,"output_tokens":2,"cached_tokens":null}],
         "by_session":[{"id":"main","title":"Plan the trip","cost":20.0,"runs":80}],
         "by_schedule":[{"id":3,"name":"Morning check","cost":5.2,"runs":30},{"id":4,"name":null,"cost":1.0,"runs":2}],
         "budget":{"monthly_usd":50.0,"spent":41.20,"fraction":0.824,"paused":false}}
        """
    let usage = try JSONDecoder().decode(AgentUsage.self, from: Data(json.utf8))
    #expect(usage.estimated && usage.timezone == "America/Toronto")
    #expect(usage.today == 1.23 && usage.thisMonth == 41.2 && usage.lastMonth == 38.1 && usage.allTime == 210.55)
    #expect(usage.daily.map(\.cost) == [0.42, 0] && usage.daily.first?.runs == 7)
    let day = try #require(usage.daily.first?.day)
    #expect(Calendar.current.dateComponents([.year, .month, .day], from: day) == DateComponents(year: 2026, month: 10, day: 26))
    #expect(usage.byKind.map(\.kind) == ["chat", "scheduled", "tidy_up"], "most expensive first")
    #expect(usage.byModel.first?.cachedTokens == 0, "tokens from before 0.16.0 count as 0")
    #expect(usage.bySession.first?.title == "Plan the trip")
    #expect(usage.bySchedule.map(\.name) == ["Morning check", nil])
    #expect(usage.budget?.paused == false && usage.budget?.month == nil)
}

@Test func theBudgetIsSetOrRemovedWithAnExplicitNull() throws {
    let set = try JSONEncoder().encode(BudgetSetting(usd: 50))
    #expect(String(decoding: set, as: UTF8.self) == #"{"monthly_budget_usd":50}"#)
    let removed = try JSONEncoder().encode(BudgetSetting(usd: nil))
    #expect(String(decoding: removed, as: UTF8.self) == #"{"monthly_budget_usd":null}"#)
}

@Test func aPlanAnswerSendsThePlanOnlyWhenEdited() throws {
    let plain = try JSONEncoder().encode(ApprovalAnswer(approved: true, text: nil, plan: nil))
    #expect(String(decoding: plain, as: UTF8.self) == #"{"approved":true}"#)
    let plan = [
        PlanEdit(step: "Push the branch", status: "pending", commands: ["git push -u origin fix"]),
        PlanEdit(step: "Check CI", status: "pending"),
    ]
    let edited = try JSONSerialization.jsonObject(
        with: JSONEncoder().encode(ApprovalAnswer(approved: true, text: nil, plan: plan))) as? [String: Any]
    #expect(edited?.keys.sorted() == ["approved", "plan"])
    let steps = try #require(edited?["plan"] as? [[String: Any]])
    #expect(steps[0]["step"] as? String == "Push the branch" && steps[0]["commands"] as? [String] == ["git push -u origin fix"])
    // A need it doesn't have isn't sent at all.
    #expect(steps[1].keys.sorted() == ["status", "step"])
}

@Test func whatTheUserAddsToAPlanGoesWithTheAnswerOnlyWhenThereIsSomething() throws {
    let noted = try JSONSerialization.jsonObject(
        with: JSONEncoder().encode(ApprovalAnswer(approved: false, text: nil, note: "Skip Slack"))) as? [String: Any]
    #expect(noted?.keys.sorted() == ["approved", "note"] && noted?["note"] as? String == "Skip Slack")
    let plain = try JSONEncoder().encode(ApprovalAnswer(approved: true, text: nil, note: nil))
    #expect(String(decoding: plain, as: UTF8.self) == #"{"approved":true}"#)
}

// MARK: Model keys (agentd 0.20.0)

@Test func decodesTheProvidersAndTheKeyTheDesktopNeeds() throws {
    let json = """
        {"desktop":"d","busy":false,"settings":{"model":"claude-opus-4-6","allow_actions":true},"last_seq":1,
         "known_models":["zai-org/GLM-5.3"],"shell_tool":true,"version":"0.20.0","provider":"Anthropic",
         "model_providers":[
           {"id":"anthropic","name":"Anthropic","models":["claude-opus-4-6","claude-opus-5-5"],"helper":"claude-haiku-5-5","key":null},
           {"id":"together","name":"Together","models":["zai-org/GLM-5.3"],"helper":"zai-org/GLM-5.3-Flash","key":"deployment"}],
         "needs_model_key":{"id":"anthropic","name":"Anthropic"}}
        """
    let status = try JSONDecoder().decode(AgentStatus.self, from: Data(json.utf8))
    #expect(status.needsModelKey == ProviderRef(id: "anthropic", name: "Anthropic"))
    #expect(status.modelProviders?.map(\.hasKey) == [false, true])
    #expect(status.provider(of: "claude-opus-5-5")?.id == "anthropic")
    #expect(status.provider(of: "zai-org/GLM-5.3-Flash")?.id == "together", "a helper is its provider's too")
    #expect(status.provider(of: "gpt-9") == nil)
    #expect(status.needsModelKey?.keyLabel == "Anthropic API key")
    #expect(ProviderRef(id: "together", name: "Together").keyLabel == "Together AI API key")
    // Older desktops have neither field.
    let old = try JSONDecoder().decode(
        AgentStatus.self,
        from: Data(#"{"desktop":"d","busy":false,"settings":{"model":"m","allow_actions":true},"last_seq":1,"known_models":["m"],"shell_tool":true,"version":"0.19.2"}"#.utf8))
    #expect(old.modelProviders == nil && old.needsModelKey == nil && old.provider(of: "m") == nil)
    let saved = try JSONDecoder().decode(
        SavedModelKey.self,
        from: Data(#"{"checked":false,"providers":[{"id":"anthropic","name":"Anthropic","models":[],"helper":"h","key":"app"}]}"#.utf8))
    #expect(!saved.checked && saved.providers.first?.key == "app")
}

@Test func anErrorsWordsComeThroughWithOtherFieldsBesideThem() throws {
    // A 409 for a message sent before the key: `needs_model_key` is an object.
    let body = #"{"error":"Your agent needs your Anthropic API key before it can work","needs_model_key":{"id":"anthropic","name":"Anthropic"}}"#
    let response = HTTPURLResponse(url: URL(string: "http://d")!, statusCode: 409, httpVersion: nil, headerFields: nil)!
    #expect(throws: AgentError.http(409, "Your agent needs your Anthropic API key before it can work")) {
        try AgentClient.check(response, Data(body.utf8))
    }
}
