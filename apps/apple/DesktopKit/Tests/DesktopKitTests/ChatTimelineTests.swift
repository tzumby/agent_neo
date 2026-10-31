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
    #expect(timeline.items.count == 4)  // user, work, terminal, terminal (with its approval)
    guard case .work(let work) = timeline.items[1] else { Issue.record("no work item"); return }
    #expect(work.steps.map(\.tool) == ["run_shell", "read_text", "run_shell"])
    #expect(work.steps.last?.asRoot == true)
    guard case .terminal(let whoami) = timeline.items[2], case .terminal(let root) = timeline.items[3] else {
        Issue.record("no terminals")
        return
    }
    #expect(whoami.state == .running)
    #expect(root.state == .waitingForApproval && root.asRoot)
    #expect(root.approval?.state == .pending && root.approval?.reason == "runs as root")

    timeline.apply(try event(8, "approval.resolved", #"{"task":"t1","id":"a1","approved":true}"#))
    timeline.apply(
        try event(
            85, "shell.output",
            #"{"task":"t1","command":"sudo id -u","exit_code":0,"timed_out":false,"output":"0","seconds":0.1}"#))
    guard case .terminal(let ran) = timeline.items[3] else { Issue.record("no terminal"); return }
    #expect(ran.state == .finished && ran.exitCode == 0 && ran.output == "0")
    timeline.apply(try event(9, "message", #"{"role":"assistant","text":"agent; 0","task":"t1"}"#))
    timeline.apply(
        try event(
            10, "task.finished",
            #"{"task":"t1","status":"done","steps":2,"seconds":3,"cost":0.0038,"usage":{"input":100,"cached":0,"output":20}}"#
        ))
    #expect(!timeline.busy)
    guard case .terminal(let approved) = timeline.items[3] else { Issue.record("no terminal"); return }
    #expect(approved.approval?.state == .approved)
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

@Test func deniedOrUnfinishedCommands() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(try event(2, "shell", #"{"task":"t1","command":"sudo reboot","as_root":true,"needs_approval":true}"#))
    timeline.apply(try event(3, "approval.requested", #"{"task":"t1","id":"a1","command":"sudo reboot","reason":"runs as root"}"#))
    timeline.apply(try event(4, "approval.resolved", #"{"task":"t1","id":"a1","approved":false}"#))
    timeline.apply(try event(5, "shell", #"{"task":"t1","command":"sleep 999","as_root":false,"needs_approval":false}"#))
    timeline.apply(try event(6, "task.finished", #"{"task":"t1","status":"cancelled","steps":2,"seconds":9}"#))
    guard case .terminal(let denied) = timeline.items[1], case .terminal(let stopped) = timeline.items[2] else {
        Issue.record("no terminals")
        return
    }
    #expect(denied.state == .denied && denied.approval?.state == .denied)
    #expect(stopped.state == .didNotFinish)

    // History from an agentd without output events: the command ran, nothing was recorded.
    timeline.apply(try event(7, "task.started", #"{"task":"t2","model":"m"}"#))
    timeline.apply(try event(8, "shell", #"{"task":"t2","command":"whoami","as_root":false,"needs_approval":false}"#))
    timeline.apply(try event(9, "task.finished", #"{"task":"t2","status":"done","steps":1,"seconds":1}"#))
    guard case .terminal(let old) = timeline.items.last else { Issue.record("no terminal"); return }
    #expect(old.state == .unrecorded)
}

@Test func browserApprovalsAreTheirOwnCards() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(
        try event(
            2, "approval.requested",
            #"{"task":"t1","id":"b1","kind":"browser","action":"type","target":"Message to Alice","text":"Hi Alice!","submit":false,"site":"app.slack.com","reason":"writes on the page"}"#
        ))
    timeline.apply(try event(3, "approval.resolved", #"{"task":"t1","id":"b1","approved":true}"#))
    guard case .approval(let approval) = timeline.items.last else { Issue.record("no approval card"); return }
    #expect(approval.state == .approved)
    #expect(approval.browser?.action == "type")
    #expect(approval.browser?.target == "Message to Alice")
    #expect(approval.browser?.text == "Hi Alice!")
    #expect(approval.browser?.site == "app.slack.com")
}
