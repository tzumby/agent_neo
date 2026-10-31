import Foundation
import Testing

@testable import AgentAPI
@testable import ChatCore

private func event(_ seq: Int, _ kind: String, _ json: String) throws -> AgentEvent {
    AgentEvent(seq: seq, kind: kind, data: try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8)))
}

@Test func parsesServerSentEventLines() throws {
    let line = #"data: {"seq":7,"ts":"2026-10-31T03:00:00Z","kind":"step","data":{"task":"t1","tool":"read_text"}}"#
    let parsed = try #require(try SSE.event(fromLine: line))
    #expect(parsed.seq == 7)
    #expect(parsed.data["tool"]?.string == "read_text")
    #expect(try SSE.event(fromLine: ": keep-alive") == nil)
    #expect(try SSE.event(fromLine: "event: step") == nil)
}

@Test func buildsATaskWithStepsApprovalAndAnswer() throws {
    var timeline = ChatTimeline()
    let events = [
        try event(1, "message", #"{"role":"user","text":"whoami then sudo id -u","task":"t1"}"#),
        try event(2, "task.started", #"{"task":"t1","model":"zai-org/GLM-5.3"}"#),
        try event(3, "step", #"{"task":"t1","step":1,"tool":"run_shell","args":"{}"}"#),
        try event(4, "shell", #"{"task":"t1","command":"whoami","as_root":false,"needs_approval":false}"#),
        try event(5, "step", #"{"task":"t1","step":2,"tool":"read_text","args":"{}"}"#),
        try event(6, "shell", #"{"task":"t1","command":"sudo id -u","as_root":true,"needs_approval":true}"#),
        try event(7, "approval.requested", #"{"task":"t1","id":"a1","command":"sudo id -u","reason":"runs as root"}"#),
    ]
    events.forEach { timeline.apply($0) }
    #expect(timeline.busy)
    #expect(timeline.items.count == 3)  // user, work, approval
    guard case .work(let work) = timeline.items[1] else { Issue.record("no work item"); return }
    #expect(work.steps.map(\.tool) == ["run_shell", "read_text", "run_shell"])
    #expect(work.steps.last?.asRoot == true)

    timeline.apply(try event(8, "approval.resolved", #"{"task":"t1","id":"a1","approved":true}"#))
    timeline.apply(try event(9, "message", #"{"role":"assistant","text":"agent; 0","task":"t1"}"#))
    timeline.apply(
        try event(
            10, "task.finished",
            #"{"task":"t1","status":"done","steps":2,"seconds":3,"cost":0.0038,"usage":{"input":100,"cached":0,"output":20}}"#
        ))
    #expect(!timeline.busy)
    guard case .approval(let approval) = timeline.items[2] else { Issue.record("no approval"); return }
    #expect(approval.state == .approved)
    guard case .work(let finished) = timeline.items[1] else { Issue.record("no work item"); return }
    #expect(finished.finished?.cost == 0.0038)
    #expect(timeline.items.last == .assistant(Message(id: 9, text: "agent; 0")))
}

@Test func actionNeededIsResolvedOrExpired() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "action_needed",
            #"{"task":"t1","id":"x1","reason":"Sign in to Slack","page":"Slack | https://slack.com","image":"/api/v1/images/ab"}"#
        ))
    timeline.apply(try event(2, "action_needed", #"{"task":"t2","id":"x2","reason":"2FA","page":"","image":null}"#))
    timeline.apply(try event(3, "action.resolved", #"{"id":"x1","choice":"done"}"#))
    timeline.expire(action: "x2")
    guard case .action(let first) = timeline.items[0], case .action(let second) = timeline.items[1] else {
        Issue.record("no actions")
        return
    }
    #expect(first.state == .done)
    #expect(first.imagePath == "/api/v1/images/ab")
    #expect(second.state == .expired)
    #expect(second.imagePath == nil)
}
