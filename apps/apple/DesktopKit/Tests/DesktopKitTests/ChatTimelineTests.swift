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

/// A task with steps, a command, a save and an approval.
private func aTask() throws -> [AgentEvent] {
    [
        try event(1, "message", #"{"role":"user","text":"whoami then sudo id -u","task":"t1"}"#),
        try event(2, "task.started", #"{"task":"t1","model":"zai-org/GLM-5.3"}"#),
        try event(3, "step", #"{"task":"t1","step":1,"tool":"run_shell","args":"{}"}"#),
        try event(4, "shell", #"{"task":"t1","command":"whoami","as_root":false,"needs_approval":false}"#),
        try event(5, "shell.output", #"{"task":"t1","command":"whoami","exit_code":0,"output":"worker"}"#),
        try event(6, "memory.saved", #"{"task":"t1","id":7,"text":"User is worker","kind":"fact","trusted":true}"#),
        try event(7, "schedule.created", #"{"task":"t1","id":4,"name":"Inbox","prompt":"p","description":"d"}"#),
        try event(8, "approval.requested", #"{"task":"t1","id":"a1","command":"sudo id -u","reason":"runs as root"}"#),
        try event(9, "task.paused", #"{"task":"t1","approval":"a1"}"#),
        try event(10, "approval.resolved", #"{"task":"t1","id":"a1","approved":true}"#),
        try event(11, "task.resumed", #"{"task":"t1"}"#),
        try event(12, "message", #"{"role":"assistant","text":"worker; 0","task":"t1"}"#),
        try event(13, "task.finished", #"{"task":"t1","status":"done","steps":2,"seconds":3,"cost":0.0038}"#),
    ]
}

@Test func onlyTheConversationAndWhatNeedsTheUserShow() throws {
    let events = try aTask()
    var timeline = ChatTimeline()
    events[..<7].forEach { timeline.apply($0) }
    // Working: the user's message, and nothing of the steps, the command or the saves.
    #expect(timeline.busy)
    #expect(timeline.visibleItems == [.user(Message(id: 1, text: "whoami then sudo id -u", task: "t1"))])

    // Waiting for an approval: the card shows, and the desktop is free.
    events[7..<9].forEach { timeline.apply($0) }
    #expect(!timeline.busy)
    guard case .approval(let card) = timeline.visibleItems.last else {
        Issue.record("no approval card")
        return
    }
    #expect(card.command == "sudo id -u" && card.reason == "runs as root")

    // Answered: the card leaves; the answer ends the task.
    events[9...].forEach { timeline.apply($0) }
    #expect(!timeline.busy)
    #expect(
        timeline.visibleItems == [
            .user(Message(id: 1, text: "whoami then sudo id -u", task: "t1")),
            .assistant(Message(id: 12, text: "worker; 0", task: "t1")),
        ])
}

@Test func theLaterPartOfTheLogGivesTheSameChat() throws {
    let events = try aTask()
    let whole = ChatTimeline(replaying: events)
    // The newest page starts after the approval was asked for: its answer is ignored.
    let newest = ChatTimeline(replaying: Array(events[9...]))
    #expect(newest.visibleItems == [.assistant(Message(id: 12, text: "worker; 0", task: "t1"))])
    // The older page goes in front: the same chat as from the start.
    #expect(ChatTimeline(replaying: Array(events[..<9]) + Array(events[9...])) == whole)
    // A page that ends mid-task is working.
    #expect(ChatTimeline(replaying: Array(events[..<5])).busy)
}

@Test func actionNeededLeavesOnceResolvedOrExpired() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "action_needed",
            #"{"task":"t1","id":"x1","reason":"Sign in to Slack","page":"Slack | https://slack.com","image":"/api/v1/images/ab"}"#
        ))
    timeline.apply(try event(2, "action_needed", #"{"task":"t2","id":"x2","reason":"2FA","page":"","image":null}"#))
    guard case .action(let first) = timeline.visibleItems.first else {
        Issue.record("no action card")
        return
    }
    #expect(first.imagePath == "/api/v1/images/ab" && first.state == .pending)
    timeline.apply(try event(3, "action.resolved", #"{"id":"x1","choice":"done"}"#))
    timeline.expire(action: "x2")
    guard case .action(let done) = timeline.items[0], case .action(let expired) = timeline.items[1] else {
        Issue.record("no actions")
        return
    }
    #expect(done.state == .done && expired.state == .expired)
    #expect(timeline.visibleItems.isEmpty)
}

@Test func stoppingShowsAtOnce() throws {
    var timeline = ChatTimeline()
    timeline.markStopping()
    #expect(!timeline.stopping)  // nothing to stop
    timeline.apply(try event(1, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(try event(2, "approval.requested", #"{"task":"t1","id":"a1","command":"sleep 9","reason":"r"}"#))
    timeline.markStopping()
    #expect(timeline.stopping)
    timeline.apply(try event(3, "task.stopping", "{}"))
    timeline.apply(try event(4, "approval.resolved", #"{"task":"t1","id":"a1","approved":false,"expired":true}"#))
    timeline.apply(try event(5, "message", #"{"role":"assistant","text":"Stopped.","task":"t1"}"#))
    timeline.apply(try event(6, "task.finished", #"{"task":"t1","status":"cancelled","steps":2,"seconds":9}"#))
    #expect(!timeline.stopping && !timeline.busy)
    #expect(timeline.visibleItems == [.assistant(Message(id: 5, text: "Stopped.", task: "t1"))])
}

@Test func browserAndScheduleCardsShowWhileTheyWait() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(
        try event(
            2, "approval.requested",
            #"{"task":"t1","id":"b1","kind":"browser","action":"type","target":"Message to Alice","text":"Hi Alice!","submit":false,"sends":true,"site":"app.slack.com","reason":"writes on the page"}"#
        ))
    guard case .approval(let browser) = timeline.visibleItems.last else {
        Issue.record("no browser card")
        return
    }
    #expect(browser.browser?.target == "Message to Alice" && browser.browser?.sends == true)
    timeline.apply(try event(3, "approval.resolved", #"{"task":"t1","id":"b1","approved":true}"#))
    #expect(timeline.visibleItems.isEmpty)

    timeline.apply(
        try event(
            4, "approval.requested",
            #"{"kind":"schedule","id":"a2","task":"t1","name":"Forward","text":"Forward my email","when":"Every day at 09:00","reason":"r"}"#
        ))
    guard case .approval(let schedule) = timeline.visibleItems.last else {
        Issue.record("no schedule card")
        return
    }
    #expect(schedule.schedule == ScheduleProposal(name: "Forward", prompt: "Forward my email", when: "Every day at 09:00"))
    timeline.apply(
        try event(5, "approval.resolved", #"{"task":"t1","id":"a2","approved":false,"expired":true,"reason":"policy_changed"}"#))
    #expect(timeline.visibleItems.isEmpty)
}

@Test func keyRequestsWaitUntilAnswered() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "message", #"{"role":"user","text":"Clone acme/api","task":"t1"}"#))
    timeline.apply(try event(2, "task.started", #"{"task":"t1","model":"m"}"#))
    timeline.apply(
        try event(
            3, "approval.requested",
            #"{"kind":"secret","id":"s1","task":"t1","name":"GITHUB_TOKEN","label":"GitHub token","why":"To clone acme/api","how":"1. Open the link","url":"https://github.com/settings/personal-access-tokens/new?contents=read","host":"github.com","replace":false,"tainted":true}"#
        ))
    timeline.apply(try event(4, "task.paused", #"{"task":"t1","approval":"s1"}"#))
    guard case .approval(let approval) = timeline.visibleItems.last, let request = approval.secret else {
        Issue.record("no key request")
        return
    }
    #expect((request.name, request.label, request.host, request.tainted) == ("GITHUB_TOKEN", "GitHub token", "github.com", true))
    #expect(request.url?.host() == "github.com")

    // Saved while another task ran: the card stays, saying so.
    timeline.apply(try event(5, "secret.saved", #"{"name":"GITHUB_TOKEN","label":"GitHub token"}"#))
    guard case .approval(let saved) = timeline.visibleItems.last else { return }
    #expect(saved.secret?.saved == true && saved.state == .pending)

    timeline.apply(try event(6, "approval.resolved", #"{"task":"t1","id":"s1","approved":true}"#))
    timeline.apply(try event(7, "task.resumed", #"{"task":"t1"}"#))
    #expect(timeline.busy)
    #expect(timeline.visibleItems.count == 1)  // the user's message
}

@Test func scheduledRunsAreLabelledAndQuietAnswersCollapse() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(1, "message", #"{"role":"user","text":"Summarize my inbox","task":"t2","schedule":{"id":4,"name":"Inbox"}}"#))
    timeline.apply(try event(2, "message", #"{"role":"assistant","text":"Inbox is clear.","task":"t2","quiet":true}"#))
    timeline.apply(try event(3, "message", #"{"role":"assistant","text":"Done.","task":"t3"}"#))
    guard case .user(let prompt) = timeline.items[0], case .assistant(let quiet) = timeline.items[1],
        case .assistant(let loud) = timeline.items[2]
    else {
        Issue.record("no messages")
        return
    }
    #expect(prompt.schedule == "Inbox" && prompt.task == "t2")
    #expect(quiet.quiet && !loud.quiet)
}

@Test func aReportFromARunThatFoundNothingUrgentIsShownInFull() throws {
    var timeline = ChatTimeline()
    // JSON-escaped newlines: a short report with a list, as a run writes one.
    let report = #"Checked the three blogs.\n\n- **One:** a new post\n- **Two:** nothing\n\nNothing urgent."#
    timeline.apply(try event(1, "message", #"{"role":"assistant","text":"\#(report)","task":"t2","quiet":true}"#))
    timeline.apply(try event(2, "message", #"{"role":"assistant","text":"Nothing new.","task":"t3","quiet":true}"#))
    guard case .assistant(let full) = timeline.items[0], case .assistant(let line) = timeline.items[1] else {
        Issue.record("no messages")
        return
    }
    #expect(!full.quiet && full.text.contains("\n- **One:**"))
    #expect(line.quiet)
}

@Test func commandCardsCarryPlainWords() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"kind":"shell","id":"a1","task":"t1","command":"env | grep -i proxy; for h in a b; do curl -sI $h; done","reason":"could send data off the desktop","summary":"Check which proxy the shell uses","tainted":true}"#
        ))
    // From an agentd before 0.15.2, or a model that left the words out.
    timeline.apply(
        try event(2, "approval.requested", #"{"id":"a2","task":"t2","command":"sudo id -u","reason":"runs as root","summary":""}"#))
    guard case .approval(let worded) = timeline.items[0], case .approval(let bare) = timeline.items[1] else {
        Issue.record("no cards")
        return
    }
    #expect(worded.summary == "Check which proxy the shell uses" && worded.tainted)
    #expect(worded.command.hasPrefix("env | grep"))
    #expect(bare.summary == nil && !bare.tainted)
}
