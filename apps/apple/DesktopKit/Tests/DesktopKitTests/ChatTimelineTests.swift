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
        try event(41, "shell.output", #"{"task":"t1","command":"whoami","exit_code":0,"timed_out":false,"output":"worker"}"#),
        try event(5, "step", #"{"task":"t1","step":2,"tool":"read_text","args":"{}"}"#),
        try event(6, "shell", #"{"task":"t1","command":"sudo id -u","as_root":true,"needs_approval":true}"#),
        try event(7, "approval.requested", #"{"task":"t1","id":"a1","command":"sudo id -u","reason":"runs as root"}"#),
    ]
    events.forEach { timeline.apply($0) }
    #expect(timeline.busy)
    // user, work, and the pending approval; no terminal cards.
    #expect(timeline.visibleItems.count == 3)
    guard case .work(let work) = timeline.items[1], case .approval(let pending) = timeline.items[2] else {
        Issue.record("no work or approval")
        return
    }
    #expect(work.steps.map(\.tool) == ["run_shell", "read_text", "run_shell"])
    #expect(work.steps.first?.run?.state == .finished && work.steps.first?.run?.output == "worker")
    #expect(work.steps.last?.asRoot == true && work.steps.last?.run?.state == .waitingForApproval)
    #expect(pending.state == .pending && pending.reason == "runs as root" && pending.browser == nil)

    // Answered: the card leaves the chat, the step runs and gets its output.
    timeline.apply(try event(8, "approval.resolved", #"{"task":"t1","id":"a1","approved":true}"#))
    #expect(timeline.visibleItems.count == 2)
    guard case .work(let running) = timeline.items[1] else { Issue.record("no work item"); return }
    #expect(running.steps.last?.run?.state == .running)
    timeline.apply(
        try event(
            85, "shell.output",
            #"{"task":"t1","command":"sudo id -u","exit_code":0,"timed_out":false,"output":"0","seconds":0.1}"#))
    timeline.apply(try event(9, "message", #"{"role":"assistant","text":"worker; 0","task":"t1"}"#))
    timeline.apply(
        try event(
            10, "task.finished",
            #"{"task":"t1","status":"done","steps":2,"seconds":3,"cost":0.0038,"usage":{"input":100,"cached":0,"output":20}}"#
        ))
    #expect(!timeline.busy)
    guard case .work(let finished) = timeline.items[1] else { Issue.record("no work item"); return }
    #expect(finished.steps.last?.run == ShellRun(state: .finished, exitCode: 0, output: "0", seconds: 0.1))
    #expect(finished.finished?.cost == 0.0038)
    #expect(timeline.items.last == .assistant(Message(id: 9, text: "worker; 0")))
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
    timeline.markStopping()
    #expect(timeline.stopping)
    timeline.apply(try event(6, "task.stopping", "{}"))
    timeline.apply(try event(7, "task.finished", #"{"task":"t1","status":"cancelled","steps":2,"seconds":9}"#))
    #expect(!timeline.stopping)
    guard case .work(let work) = timeline.items[0] else { Issue.record("no work"); return }
    #expect(work.steps.map { $0.run?.state } == [.denied, .didNotFinish])
    #expect(timeline.visibleItems.count == 1)  // the answered approval is gone from the chat

    // History from an agentd without output events: the command ran, nothing was recorded.
    timeline.apply(try event(11, "task.started", #"{"task":"t3","model":"m"}"#))
    timeline.apply(try event(12, "shell", #"{"task":"t3","command":"whoami","as_root":false,"needs_approval":false}"#))
    timeline.apply(try event(13, "task.finished", #"{"task":"t3","status":"done","steps":1,"seconds":1}"#))
    guard case .work(let old) = timeline.items.last else { Issue.record("no work item"); return }
    #expect(old.steps.last?.run?.state == .unrecorded)
}

@Test func browserApprovalsAreTheirOwnCards() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(
        try event(
            2, "approval.requested",
            #"{"task":"t1","id":"b1","kind":"browser","action":"type","target":"Message to Alice","text":"Hi Alice!","submit":false,"sends":true,"site":"app.slack.com","reason":"writes on the page"}"#
        ))
    timeline.apply(try event(3, "approval.resolved", #"{"task":"t1","id":"b1","approved":true}"#))
    guard case .approval(let approval) = timeline.items.last else { Issue.record("no approval card"); return }
    #expect(approval.state == .approved)
    #expect(approval.browser?.action == "type")
    #expect(approval.browser?.target == "Message to Alice")
    #expect(approval.browser?.text == "Hi Alice!")
    #expect(approval.browser?.site == "app.slack.com")
    #expect(approval.browser?.sends == true)
    // Browser approvals stay in the chat after they're answered (what was sent, and where).
    #expect(timeline.visibleItems.count == 2)
}

@Test func aTaskPausesForApprovalAndResumes() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(try event(2, "shell", #"{"task":"t1","command":"sudo apt update","as_root":true,"needs_approval":true}"#))
    timeline.apply(try event(3, "approval.requested", #"{"task":"t1","id":"a1","command":"sudo apt update","reason":"runs as root"}"#))
    timeline.apply(try event(4, "task.paused", #"{"task":"t1","approval":"a1"}"#))
    #expect(!timeline.busy)
    guard case .work(let paused) = timeline.items[0] else { Issue.record("no work item"); return }
    #expect(paused.waiting && paused.finished == nil)

    // Answered (maybe after a restart): the same task carries on, the step fills in.
    timeline.apply(try event(5, "approval.resolved", #"{"task":"t1","id":"a1","approved":true}"#))
    timeline.apply(try event(6, "task.resumed", #"{"task":"t1"}"#))
    #expect(timeline.busy)
    timeline.apply(
        try event(7, "shell.output", #"{"task":"t1","command":"sudo apt update","exit_code":0,"timed_out":false,"output":"ok"}"#))
    guard case .work(let resumed) = timeline.items[0] else { Issue.record("no work item"); return }
    #expect(!resumed.waiting)
    #expect(resumed.steps.last?.run?.state == .finished)

    // Given up on while paused: the card expires and the command never ran.
    timeline.apply(try event(8, "shell", #"{"task":"t1","command":"sudo reboot","as_root":true,"needs_approval":true}"#))
    timeline.apply(try event(9, "approval.requested", #"{"task":"t1","id":"a2","command":"sudo reboot","reason":"runs as root"}"#))
    timeline.apply(try event(10, "task.paused", #"{"task":"t1","approval":"a2"}"#))
    timeline.apply(try event(11, "approval.resolved", #"{"task":"t1","id":"a2","approved":false,"expired":true}"#))
    timeline.apply(try event(12, "task.finished", #"{"task":"t1","status":"cancelled","steps":3,"seconds":5}"#))
    guard case .work(let ended) = timeline.items[0], case .approval(let expired) = timeline.items[2] else {
        Issue.record("no work or approval")
        return
    }
    #expect(expired.state == .expired && ended.steps.last?.run?.state == .didNotFinish)
}

@Test func schedulesShowAsNotesAndScheduledRunsAreLabelled() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "message", #"{"role":"user","text":"Every weekday at 8, summarize my inbox","task":"t1"}"#))
    timeline.apply(try event(2, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(
        try event(
            3, "schedule.created",
            #"{"task":"t1","id":4,"name":"Inbox","prompt":"Summarize my inbox","description":"Weekdays at 08:00","next_run":1793710800}"#
        ))
    guard case .schedule(let note) = timeline.items.last else {
        Issue.record("no schedule note")
        return
    }
    #expect((note.id, note.name, note.when) == (4, "Inbox", "Weekdays at 08:00"))
    #expect(note.nextRun == Date(timeIntervalSince1970: 1_793_710_800))
    timeline.apply(try event(4, "schedule.deleted", #"{"id":4}"#))
    guard case .schedule(let cancelled) = timeline.items.last else { return }
    #expect(cancelled.cancelled)

    // A scheduled run's prompt carries the schedule's name.
    timeline.apply(
        try event(5, "message", #"{"role":"user","text":"Summarize my inbox","task":"t2","schedule":{"id":4,"name":"Inbox"}}"#))
    guard case .user(let message) = timeline.items.last else { return }
    #expect(message.schedule == "Inbox")

    // Asked to approve a schedule: its own card, not a command, hidden once answered.
    timeline.apply(
        try event(
            6, "approval.requested",
            #"{"kind":"schedule","id":"a1","task":"t3","name":"Forward","text":"Forward my email","when":"Every day at 09:00","reason":"r"}"#
        ))
    guard case .approval(let approval) = timeline.items.last else { return }
    #expect(approval.schedule == ScheduleProposal(name: "Forward", prompt: "Forward my email", when: "Every day at 09:00"))
    #expect(!approval.isCommand)
    timeline.apply(try event(7, "approval.resolved", #"{"task":"t3","id":"a1","approved":false}"#))
    #expect(!timeline.visibleItems.contains { if case .approval = $0 { true } else { false } })
}

@Test func keyRequestsAreCardsThatStayAndCommandsShowTheirKeys() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "message", #"{"role":"user","text":"Clone acme/api","task":"t1"}"#))
    timeline.apply(try event(2, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(
        try event(
            3, "approval.requested",
            #"{"kind":"secret","id":"s1","task":"t1","name":"GITHUB_TOKEN","label":"GitHub token","why":"To clone acme/api","how":"1. Open the link","url":"https://github.com/settings/personal-access-tokens/new?contents=read","host":"github.com","replace":false,"tainted":true}"#
        ))
    timeline.apply(try event(4, "task.paused", #"{"task":"t1","approval":"s1"}"#))
    guard case .approval(let approval) = timeline.items.last, let request = approval.secret else {
        Issue.record("no key request")
        return
    }
    #expect(!approval.isCommand)
    #expect((request.name, request.label, request.host, request.tainted) == ("GITHUB_TOKEN", "GitHub token", "github.com", true))
    #expect(request.url?.host() == "github.com")

    // Saved while another task ran: the card shows it's saved, still waiting.
    timeline.apply(try event(5, "secret.saved", #"{"name":"GITHUB_TOKEN","label":"GitHub token"}"#))
    guard case .approval(let saved) = timeline.items.last else { return }
    #expect(saved.secret?.saved == true && saved.state == .pending)

    // Answered: the card stays, showing the outcome (unlike command approvals).
    timeline.apply(try event(6, "approval.resolved", #"{"task":"t1","id":"s1","approved":true}"#))
    timeline.apply(try event(7, "task.resumed", #"{"task":"t1"}"#))
    #expect(timeline.visibleItems.contains { if case .approval(let a) = $0 { a.state == .approved } else { false } })

    // A command that got the key says so (by name).
    timeline.apply(
        try event(
            8, "shell",
            #"{"task":"t1","command":"git clone https://github.com/acme/api","as_root":false,"needs_approval":false,"secrets":["GITHUB_TOKEN"]}"#
        ))
    guard case .work(let work) = timeline.items[1] else { return }
    #expect(work.steps.last?.secrets == ["GITHUB_TOKEN"])
}
