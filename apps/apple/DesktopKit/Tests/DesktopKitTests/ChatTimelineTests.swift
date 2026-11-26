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

@Test func aClickCardKeepsWhatThePageShows() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"task":"t1","id":"c1","kind":"browser","action":"click","target":"Confirm","role":"button","site":"acme.com","summary":"Pay the March invoice","page":"Invoices","url":"https://acme.com/invoices?month=march","destination":"https://acme.com/confirm","around":"March, $120 Confirm","image":"/api/v1/images/ab"}"#
        ))
    guard case .approval(let card) = timeline.visibleItems.last, let action = card.browser else {
        Issue.record("no click card")
        return
    }
    #expect(card.summary == "Pay the March invoice")
    #expect(action.role == "button" && action.page == "Invoices")
    #expect(action.url == "https://acme.com/invoices?month=march")
    #expect(action.destination == "https://acme.com/confirm")
    #expect(action.around == "March, $120 Confirm" && action.imagePath == "/api/v1/images/ab")
}

@Test func aClickInANativeAppKeepsWhatTheAppShows() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"task":"t1","id":"c9","kind":"app","action":"click","app":"Mousepad","window":"~/notes.txt - Mousepad","target":"Save","role":"menuitem","around":"menu “File” › menuitem “Save” — Save the current document","summary":"Save the shopping list","reason":"may change or send something in the app","image":"/api/v1/images/ab12","binding":{"pid":4242},"tainted":true}"#
        ))
    guard case .approval(let card) = timeline.visibleItems.last, let action = card.app else {
        Issue.record("no app card")
        return
    }
    #expect(
        action
            == AppAction(
                action: "click", app: "Mousepad", window: "~/notes.txt - Mousepad", target: "Save", role: "menuitem",
                text: nil, key: nil, around: "menu “File” › menuitem “Save” — Save the current document",
                imagePath: "/api/v1/images/ab12"))
    #expect(card.summary == "Save the shopping list" && card.reason == "may change or send something in the app")
    #expect(card.tainted && !card.editable && card.command.isEmpty && card.browser == nil)
    // An older card has no steps.
    #expect(action.steps.isEmpty && action.frame == nil && !action.listsSteps && action.marks.isEmpty)
    timeline.apply(
        try event(2, "approval.requested", #"{"id":"c10","kind":"app","action":"click","target":null,"around":null,"summary":null,"image":null}"#))
    guard case .approval(let bare) = timeline.visibleItems.last, let nameless = bare.app else {
        Issue.record("no nameless app card")
        return
    }
    #expect(nameless.target.isEmpty && nameless.around.isEmpty && nameless.imagePath == nil && bare.summary == nil)
    #expect(
        [AppStep.clickTarget(role: "menuitem", name: "Save"), AppStep.clickTarget(role: "push button", name: ""),
         AppStep.clickTarget(role: "", name: "OK")] == ["Menuitem “Save”", "Push button without a name", "“OK”"])
    timeline.apply(try event(3, "approval.resolved", #"{"task":"t1","id":"c9","approved":true}"#))
    #expect(timeline.visibleItems.count == 1)
}

@Test func aBatchInANativeAppListsEveryStepWithAllItsText() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"task":"t1","id":"c11","kind":"app","action":"act","app":"Mousepad","window":"*notes.txt - Mousepad","summary":"Add milk and save","reason":"may change or send something in the app","image":"/api/v1/images/ab12","steps":[{"action":"type","target":"","role":"textbox","around":"tablist › textbox","text":"- milk\n- eggs ✓"},{"action":"key","key":"CTRL+S","window":"*notes.txt - Mousepad"},{"action":"click","target":"Save","role":"menuitem","around":"menu “File” › menuitem “Save”","pointer":true},{"action":"wait","for":"a window “Save As”","gone":true},"odd",null]}"#
        ))
    guard case .approval(let card) = timeline.visibleItems.last, let action = card.app else {
        Issue.record("no app card")
        return
    }
    #expect(action.action == "act" && action.window == "*notes.txt - Mousepad" && card.summary == "Add milk and save")
    // A batch's fields stay on its steps, and what isn't a step is left out.
    #expect(action.target.isEmpty && action.text == nil && action.frame == nil && action.steps.count == 4)
    #expect(action.listsSteps && action.marks.isEmpty)
    var typed = AppStep(action: "type")
    typed.role = "textbox"
    typed.around = "tablist › textbox"
    typed.text = "- milk\n- eggs ✓"
    #expect(action.steps[0] == typed)
    #expect(action.steps[1].key == "CTRL+S" && action.steps[1].window == "*notes.txt - Mousepad")
    #expect(action.steps[2].pointer && action.steps[3].waitFor == "a window “Save As”" && action.steps[3].gone)
    let words = action.steps.map { $0.words(window: action.window) }
    // The full text, its lines kept.
    #expect(
        words[0]
            == AppStep.Words(text: "Write in Textbox:", quote: "- milk\n- eggs ✓", note: "tablist › textbox"))
    // The card's own window isn't repeated.
    #expect(words[1] == AppStep.Words(text: "Press", key: "CTRL+S"))
    #expect(words[2].text == "Click Menuitem “Save” with the mouse" && words[2].note == "menu “File” › menuitem “Save”")
    #expect(words[3] == AppStep.Words(text: "Wait for a window “Save As” to go away", quiet: true))
    func say(_ action: String, _ set: (inout AppStep) -> Void) -> String {
        var step = AppStep(action: action)
        set(&step)
        return step.words().text
    }
    #expect(
        [
            say("set_value") { $0.role = "spinbutton"; $0.target = "Size"; $0.text = "42" },
            say("select") { $0.role = "textbox"; $0.text = "Line one" },
            say("select") { $0.role = "listitem"; $0.target = "Blue" },
            say("select") { $0.role = "cell"; $0.target = "A1"; $0.range = true },
            say("scroll") { $0.role = "list"; $0.target = "Files"; $0.direction = "down"; $0.pages = 2 },
            say("activate") { $0.window = "Save As" },
            say("close") { $0.window = "Untitled 1 - Mousepad" },
        ] == [
            "Set Spinbutton “Size” to:", "Select “Line one” in Textbox", "Select Listitem “Blue”",
            "Select from Cell “A1”…", "Scroll down 2 pages in List “Files”", "Switch to the window “Save As”",
            "Close the window “Untitled 1 - Mousepad”",
        ])
}

@Test func aPixelBatchKeepsItsPointsAndThePicturesSizeForTheMarks() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"task":"t1","id":"c12","kind":"app","action":"gui","app":"Mousepad","window":"notes.txt - Mousepad","summary":"Open the File menu and drag the file","image":"/api/v1/images/cd34","frame":{"width":1229,"height":768},"steps":[{"action":"click","x":100,"y":120.4,"button":"right","keys":["CTRL"],"key":"CTRL","window":"notes.txt - Mousepad","app":"Mousepad"},{"action":"drag","path":[{"x":10,"y":10},{"x":"far","y":5},{"x":200,"y":300}],"window":"Files","app":"Thunar"},{"action":"keypress","keys":["CTRL","S"]},{"action":"scroll","x":"400","y":300,"scroll_y":3},{"action":"wait"}]}"#
        ))
    guard case .approval(let card) = timeline.visibleItems.last, let action = card.app else {
        Issue.record("no gui card")
        return
    }
    #expect(action.action == "gui" && action.frame == AppAction.Frame(width: 1229, height: 768))
    #expect(action.imagePath == "/api/v1/images/cd34" && action.listsSteps)
    // A point that isn't a number is dropped.
    #expect(
        action.steps.map(\.points) == [
            [AppStep.Point(x: 100, y: 120.4)], [AppStep.Point(x: 10, y: 10), AppStep.Point(x: 200, y: 300)], [], [],
            [],
        ])
    #expect(action.steps[0].button == "right" && action.steps[0].key == "CTRL" && action.steps[2].key == "CTRL+S")
    let words = action.steps.map { $0.words(pixels: true, window: action.window) }
    #expect(
        words.map(\.text) == [
            "Click at 100, 120 (right button), holding CTRL", "Drag from 10, 10 to 200, 300", "Press", "Scroll",
            "Wait a second",
        ])
    #expect(words[0].window == nil && words[1].window == "In “Files”" && words[2].key == "CTRL+S" && words[4].quiet)
    // The marks: in fractions of the picture, numbered as the steps are.
    #expect(action.marks.map(\.step) == [1, 2])
    #expect(action.marks[1].points == [AppStep.Point(x: 10 / 1229, y: 10 / 768), AppStep.Point(x: 200 / 1229, y: 300 / 768)])
    timeline.apply(
        try event(
            2, "approval.requested",
            #"{"id":"c13","kind":"app","action":"gui","image":"/api/v1/images/cd34","frame":{"width":0,"height":"768"},"steps":{"action":"click"}}"#
        ))
    guard case .approval(let odd) = timeline.visibleItems.last, let oddAction = odd.app else {
        Issue.record("no odd gui card")
        return
    }
    #expect(oddAction.frame == nil && oddAction.steps.isEmpty && oddAction.marks.isEmpty)
}

@Test func anAppInstallKeepsWhatTheStoreSays() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"task":"t1","id":"c7","kind":"install","source":"Snap Store","name":"slack","app":"Slack","publisher":"Slack","verified":true,"version":"4.53.292","channel":"stable","confinement":"strict","summary":"Team communication for the 21st century.","size":233066496,"why":"So you can use Slack as a desktop app","reason":"installs an app on your desktop, as root","tainted":true}"#
        ))
    guard case .approval(let card) = timeline.visibleItems.last, let install = card.install else {
        Issue.record("no install card")
        return
    }
    #expect(
        install
            == AppInstall(
                source: "Snap Store", name: "slack", app: "Slack", publisher: "Slack", verified: true,
                version: "4.53.292", channel: "stable", confinement: "strict",
                summary: "Team communication for the 21st century.", size: 233_066_496,
                why: "So you can use Slack as a desktop app"))
    #expect(install.details == "Version 4.53.292 · stable · 233 MB")
    // The summary is the store's, not the agent's.
    #expect(card.summary == nil && card.reason == "installs an app on your desktop, as root")
    #expect(card.tainted && !card.editable && card.command.isEmpty && card.app == nil && card.browser == nil)
    timeline.apply(
        try event(2, "approval.requested", #"{"id":"c8","kind":"install","name":"slack","channel":"stable","summary":"","size":null,"why":null}"#))
    guard case .approval(let bare) = timeline.visibleItems.last, let plain = bare.install else {
        Issue.record("no bare install card")
        return
    }
    #expect(plain.app.isEmpty && !plain.verified && plain.summary.isEmpty && plain.size == nil && plain.why.isEmpty)
    #expect(plain.details == "stable" && !bare.tainted)
    #expect(
        [233_066_496, 4_240_000, 4_000_000, 1200].map(AppInstall.megabytes) == ["233 MB", "4.2 MB", "4 MB", "0.1 MB"])
    timeline.apply(try event(3, "approval.resolved", #"{"task":"t1","id":"c7","approved":true}"#))
    #expect(timeline.visibleItems.count == 1)
}

@Test func aCardSaysWhetherItTakesAnEdit() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"task":"t1","id":"c1","kind":"shell","command":"ls","reason":"x","editable":true}"#))
    guard case .approval(let card) = timeline.visibleItems.last else {
        Issue.record("no card")
        return
    }
    #expect(card.editable)
}

@Test func aShellCardKeepsTheFullCommandAndItsAccess() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"id":"a1","kind":"shell","command":"gh pr list\ngh issue list","secrets":["GITHUB_TOKEN"]}"#
        ))
    timeline.apply(
        try event(2, "approval.requested", #"{"id":"a2","kind":"shell","command":"git fetch","direct":true}"#))
    guard case .approval(let key) = timeline.visibleItems.first,
        case .approval(let direct) = timeline.visibleItems.last
    else {
        Issue.record("Missing shell approval cards")
        return
    }
    #expect(key.command == "gh pr list\ngh issue list")
    #expect(key.savedKeys == ["GITHUB_TOKEN"])
    #expect(!key.directInternet)
    #expect(direct.directInternet)
    #expect(direct.savedKeys.isEmpty)
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
    #expect(browser.editable == false)
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

private func event(_ seq: Int, _ kind: String, _ json: String, at ts: String) throws -> AgentEvent {
    AgentEvent(seq: seq, ts: ts, kind: kind, data: try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8)))
}

/// A scheduled run's prompt and answer, as agentd logs them.
private func run(_ seq: Int, _ task: String, _ schedule: String, _ answer: String, quiet: Bool) throws -> [AgentEvent] {
    let ts = "2026-11-02T1\(seq / 10 % 10):00:00.000Z"
    return [
        try event(
            seq, "message",
            #"{"role":"user","text":"Check the dashboard and report spikes","task":"\#(task)","schedule":{"id":4,"name":"\#(schedule)"}}"#,
            at: ts),
        try event(seq + 1, "task.started", #"{"task":"\#(task)","model":"m"}"#, at: ts),
        try event(
            seq + 2, "message", #"{"role":"assistant","text":"\#(answer)","task":"\#(task)","quiet":\#(quiet)}"#, at: ts),
        try event(seq + 3, "task.finished", #"{"task":"\#(task)","status":"done"}"#, at: ts),
    ]
}

@Test func aScheduledRunIsAReportNotTheUserSpeaking() throws {
    var timeline = ChatTimeline()
    let events = try run(1, "t2", "Inbox", "Inbox is clear.", quiet: true)
    events[..<2].forEach { timeline.apply($0) }
    // Working: the report's header, no answer yet, no bubble from the user.
    guard case .report(let working) = timeline.visibleItems.first else {
        Issue.record("no report")
        return
    }
    #expect(working.schedule == "Inbox" && working.instructions.hasPrefix("Check the dashboard") && working.answer == nil)
    #expect(working.ts == "2026-11-02T10:00:00.000Z")
    events[2...].forEach { timeline.apply($0) }
    timeline.apply(try event(9, "message", #"{"role":"assistant","text":"Done.","task":"t3"}"#))
    guard case .report(let done) = timeline.items[0], case .assistant(let loud) = timeline.items[1] else {
        Issue.record("no report and answer")
        return
    }
    #expect(done.answer?.text == "Inbox is clear." && done.quiet && done.task == "t2")
    #expect(!loud.quiet && timeline.items.count == 2)
}

@Test func runsThatFoundNothingFoldIntoTheLatestUntilOneFindsSomething() throws {
    var events: [AgentEvent] = []
    events += try run(10, "a1", "Spike watch", "No spike: 5.3k total", quiet: true)
    events += try run(20, "a2", "Spike watch", "No spike: 5.4k total", quiet: true)
    events += try run(30, "n1", "News", "Nothing new.", quiet: true)  // another schedule in between
    events += try run(40, "a3", "Spike watch", "No spike: 5.2k total", quiet: true)
    events += try run(50, "a4", "Spike watch", "No spike: 5.6k total", quiet: true)
    // JSON-escaped newlines: a report with a list.
    events += try run(60, "a5", "Spike watch", #"**Spike:** 41k mitigated requests\n\n- from one ASN"#, quiet: false)
    events += try run(70, "a6", "Spike watch", "Back to normal: 5.5k total", quiet: true)
    let timeline = ChatTimeline(replaying: events)
    let reports: [Report] = timeline.visibleItems.compactMap { if case .report(let r) = $0 { r } else { nil } }
    #expect(reports.map(\.task) == ["a2", "n1", "a4", "a5", "a6"])
    #expect(reports[0].earlier.map(\.task) == ["a1"] && reports[2].earlier.map(\.task) == ["a3"])
    // The folded row keeps its first run's id, so it doesn't jump as checks arrive.
    #expect(timeline.visibleItems[0].id == "report-10" && timeline.visibleItems[2].id == "report-40")
    #expect(!reports[3].quiet && reports[3].earlier.isEmpty)
    // "Open that conversation" finds a run folded into a later one.
    #expect(timeline.itemID(startingTask: "a3") == "report-40")
    #expect(timeline.itemID(startingTask: "a5") == "report-60")
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

@Test func aSiteToAllowIsACardWithItsHosts() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"kind":"access","id":"w1","task":"t1","hosts":["status.example.com"],"why":"To read the incident timeline","tainted":true}"#
        ))
    guard case .approval(let approval) = timeline.visibleItems.last, let request = approval.access else {
        Issue.record("no access card")
        return
    }
    #expect(request == AccessRequest(hosts: ["status.example.com"], why: "To read the incident timeline"))
    #expect(approval.tainted && approval.secret == nil && approval.browser == nil)
    timeline.apply(try event(2, "approval.resolved", #"{"task":"t1","id":"w1","approved":true}"#))
    #expect(timeline.visibleItems.isEmpty)
}

@Test func aScheduledRunBesideTheChatDoesNotMakeItBusy() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "run.started", #"{"task":"r1","model":"m","schedule":{"id":1,"name":"Spike watch"}}"#))
    #expect(!timeline.busy)
    timeline.apply(try event(2, "message", #"{"role":"user","text":"Hi","task":"t1"}"#))
    timeline.apply(try event(3, "task.started", #"{"task":"t1","model":"m"}"#))
    #expect(timeline.busy)
    timeline.apply(try event(4, "run.finished", #"{"task":"r1","status":"done"}"#))
    #expect(timeline.busy, "the chat's own task is still working")
}

@Test func aTasksPlanFollowsItsMessageAndFoldsWhenItEnds() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "message", #"{"role":"user","text":"Deploy the blog","task":"t1"}"#))
    timeline.apply(try event(2, "task.started", #"{"task":"t1"}"#))
    timeline.apply(
        try event(3, "plan", #"{"task":"t1","items":[{"step":"Build","status":"in_progress"},{"step":"Push","status":"pending"},{"step":"  ","status":"done"}]}"#))
    timeline.apply(try event(4, "message", #"{"role":"assistant","text":"Building…","task":"t1"}"#))
    // Each plan event is the whole plan: the latest wins, in the same row.
    timeline.apply(
        try event(5, "plan", #"{"task":"t1","items":[{"step":"Build","status":"done"},{"step":"Push","status":"weird"}]}"#))
    var shown = timeline.visibleItems.map(\.id)
    #expect(shown == ["user-1", "plan-t1", "assistant-4"])
    guard case .plan(let plan) = timeline.visibleItems[1] else { Issue.record("no plan"); return }
    #expect(plan.steps == [PlanStep(step: "Build", status: .done), PlanStep(step: "Push", status: .pending)])
    #expect(plan.done == 1)
    #expect(!plan.finished)

    timeline.apply(try event(6, "task.finished", #"{"task":"t1","status":"done"}"#))
    guard case .plan(let finished) = timeline.visibleItems[1] else { Issue.record("no plan"); return }
    #expect(finished.finished)

    // Its message is on an older page: the plan shows where it is.
    let later = ChatTimeline(replaying: [
        try event(5, "plan", #"{"task":"t1","items":[{"step":"Build","status":"done"}]}"#),
        try event(6, "task.finished", #"{"task":"t1","status":"done"}"#),
    ])
    shown = later.visibleItems.map(\.id)
    #expect(shown == ["plan-t1"])
}

@Test func aStuckTaskIsACardWithWhatItHasDone() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(1, "approval.requested", #"{"task":"t1","id":"s1","kind":"stuck","reason":"The build fails the same way each time.","minutes":33,"steps":70,"plan":{"done":2,"total":5}}"#))
    guard case .approval(let card) = try #require(timeline.visibleItems.first) else {
        Issue.record("no card")
        return
    }
    #expect(card.stuck == StuckCheck(reason: "The build fails the same way each time.", minutes: 33, steps: 70, plan: .init(done: 2, total: 5)))
    #expect(card.browser == nil && card.secret == nil)
    timeline.apply(try event(2, "approval.resolved", #"{"task":"t1","id":"s1","approved":true}"#))
    #expect(timeline.visibleItems.isEmpty)
}

@Test func workingTimeCountsFromTheTasksStart() throws {
    var timeline = ChatTimeline()
    timeline.apply(AgentEvent(seq: 1, ts: "2026-11-23T10:00:00.250Z", kind: "task.started", data: .object(["task": .string("t1")])))
    let start = try #require(timeline.workingSince)
    #expect(ChatTimeline.working(since: start, now: start.addingTimeInterval(30)) == nil)
    #expect(ChatTimeline.working(since: start, now: start.addingTimeInterval(24 * 60 + 5)) == "24 min")
    #expect(ChatTimeline.working(since: start, now: start.addingTimeInterval(65 * 60)) == "1 h 5 min")
    #expect(ChatTimeline.working(since: start, now: start.addingTimeInterval(120 * 60)) == "2 h")
    timeline.apply(AgentEvent(seq: 2, ts: "2026-11-23T10:30:00Z", kind: "task.finished", data: .object(["task": .string("t1")])))
    #expect(timeline.workingSince == nil)
}

// MARK: Plan cards

/// The plan card agentd sends (approval.requested, kind "plan").
private let planCard = """
    {"id":"a1b2c3d4e5f6","task":"t1","kind":"plan","editable":true,"tainted":false,
     "steps":[
       {"step":"Open notes.txt in Mousepad","status":"pending","app":"Mousepad"},
       {"step":"Write the list and save it","status":"pending","app":"Mousepad"},
       {"step":"Post it in #general","status":"pending","site":"app.slack.com"},
       {"step":"Push the branch","status":"pending","commands":["git push -u origin fix"]},
       {"step":"Install Slack","status":"pending","install":"slack"},
       {"step":"Tell the user what changed","status":"pending"}],
     "covers":{"sites":["app.slack.com"],"apps":["Mousepad"],"commands":["git push -u origin fix"],"installs":["slack"]}}
    """

private struct NoPlanCard: Error {}

private func planApproval(_ json: String = planCard) throws -> (Approval, PlanApproval) {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "approval.requested", json))
    guard case .approval(let card) = timeline.visibleItems.last, let plan = card.plan else { throw NoPlanCard() }
    return (card, plan)
}

/// The summary as the card says it, commands in backticks.
private func said(_ covers: PlanCovers) -> String {
    covers.phrases.map { $0.pieces.map { $0.code ? "`\($0.text)`" : $0.text }.joined() }.joined(separator: " · ")
}

@Test func aPlanCardHasItsStepsWithWhatEachNeedsAndWhatItCovers() throws {
    let (card, plan) = try planApproval()
    #expect(card.editable && !card.tainted && card.app == nil && card.browser == nil && card.command.isEmpty)
    #expect(plan.steps.count == 6)
    #expect(plan.steps[0] == PlanStep(step: "Open notes.txt in Mousepad", status: .pending, app: "Mousepad"))
    #expect(plan.steps[3].needs == [PlanNeed(kind: .command, value: "git push -u origin fix")])
    #expect(plan.steps[4].install == "slack" && plan.steps[5].needs.isEmpty)
    #expect(
        plan.covers
            == PlanCovers(
                sites: ["app.slack.com"], apps: ["Mousepad"], commands: ["git push -u origin fix"], installs: ["slack"]))
    #expect(
        said(plan.covers)
            == "clicking, typing and pressing keys on app.slack.com · input in Mousepad · running `git push -u origin fix` · installing slack"
    )
    #expect(said(PlanCovers(sites: ["a.com", "b.com", "c.com"], commands: ["ls", "pwd"]))
        == "clicking, typing and pressing keys on a.com, b.com and c.com · running `ls` and `pwd`")

    // Odd fields are left out; missing lists are empty; repeats go.
    let (odd, oddPlan) = try planApproval(
        #"{"id":"p2","kind":"plan","steps":[{"step":"Go","status":"weird","site":"","app":7,"commands":["ls","",3],"install":null},{"step":" "},"x"],"covers":{"sites":["a.com","a.com"],"apps":null}}"#
    )
    #expect(oddPlan.steps == [PlanStep(step: "Go", status: .pending, commands: ["ls"])])
    #expect(oddPlan.covers == PlanCovers(sites: ["a.com"]))
    #expect(!odd.editable, "only a card that says it takes an edit offers one")

    var timeline = ChatTimeline(replaying: [try event(1, "approval.requested", planCard)])
    timeline.apply(
        try event(2, "approval.resolved", #"{"task":"t1","id":"a1b2c3d4e5f6","approved":true,"by":"someone@example.com","edited":true,"plan":[]}"#))
    #expect(timeline.visibleItems.isEmpty, "answered, it leaves the chat")
}

@Test func thePlanChecklistKeepsEachStepsNeeds() throws {
    let timeline = ChatTimeline(replaying: [
        try event(
            1, "plan",
            #"{"task":"t1","items":[{"step":"Post it in #general","status":"in_progress","site":"app.slack.com"},{"step":"Push","status":"pending","commands":["git push","npm publish"]},{"step":"Tell the user","status":"pending"}]}"#
        )
    ])
    guard case .plan(let plan) = timeline.visibleItems.first else {
        Issue.record("no plan")
        return
    }
    #expect(plan.steps[0] == PlanStep(step: "Post it in #general", status: .inProgress, site: "app.slack.com"))
    #expect(plan.steps[1].needs.map(\.value) == ["git push", "npm publish"])
    #expect(plan.steps[2] == PlanStep(step: "Tell the user", status: .pending))
}

@Test func aPlanCardsEditsTakeNeedsOffAndOnlyAChangeIsSent() throws {
    let (_, plan) = try planApproval()
    var draft = PlanDraft(plan.steps)
    #expect(draft.edits(from: plan.steps) == nil, "as the agent wrote it: nothing to send")
    #expect(draft.problem == nil && draft.canAdd)
    draft.set("Open notes.txt in Mousepad  ", of: 0)
    #expect(draft.edits(from: plan.steps) == nil, "spaces around aren't a change")

    // Take Mousepad off one step: still on the other, so still covered.
    draft.removeNeed(0, from: 0)
    #expect(draft.steps[0].plan == PlanStep(step: "Open notes.txt in Mousepad  ", status: .pending))
    #expect(draft.covered(by: plan.covers).apps == ["Mousepad"])
    draft.removeNeed(0, from: 1)
    #expect(
        draft.covered(by: plan.covers)
            == PlanCovers(sites: ["app.slack.com"], commands: ["git push -u origin fix"], installs: ["slack"]))

    // One command at a time.
    let two = PlanStep(step: "Ship", status: .pending, site: "github.com", commands: ["git push", "npm publish"])
    #expect(two.needs.map(\.kind) == [.site, .command, .command])
    #expect(two.removing(need: 1) == PlanStep(step: "Ship", status: .pending, site: "github.com", commands: ["npm publish"]))
    #expect(two.removing(need: 1).removing(need: 1) == PlanStep(step: "Ship", status: .pending, site: "github.com"))

    // Reword, delete, add, reorder; a step's words stay one line, at most 120 characters.
    draft.set("Push the fix branch\n", of: 3)
    for id in [0, 1, 5] { draft.delete(id) }
    draft.move(3, by: -1)
    draft.move(2, by: -5)  // nowhere to go
    let added = draft.add()
    draft.set("Check CI", of: added)
    #expect(draft.steps.map(\.id) == [3, 2, 4, added])
    let edits = try #require(draft.edits(from: plan.steps))
    #expect(
        edits == [
            PlanEdit(step: "Push the fix branch", status: "pending", commands: ["git push -u origin fix"]),
            PlanEdit(step: "Post it in #general", status: "pending", site: "app.slack.com"),
            PlanEdit(step: "Install Slack", status: "pending", install: "slack"),
            PlanEdit(step: "Check CI", status: "pending"),
        ], "the whole plan, its words trimmed")
    draft.set(String(repeating: "x", count: 130), of: added)
    #expect(draft.steps.last?.plan.step.count == PlanDraft.maxLength)

    // Nothing left to cover: approving only starts the plan.
    var bare = PlanDraft(plan.steps.map { PlanStep(step: $0.step, status: $0.status) })
    #expect(bare.covered(by: plan.covers).phrases.isEmpty)
    // A later card covers only what's new, though its steps are the whole plan.
    #expect(said(PlanDraft(plan.steps).covered(by: PlanCovers(installs: ["slack"]))) == "installing slack")

    // What agentd would refuse can't be approved.
    bare.add()
    #expect(bare.problem == "Write the empty step, or delete it.")
    for step in bare.steps { bare.delete(step.id) }
    #expect(bare.problem == "A plan needs a step. Add one, or deny it.")
    for _ in 0..<PlanDraft.maxSteps {
        bare.set("x", of: bare.add())
    }
    #expect(!bare.canAdd && bare.problem == nil)
}

@Test func anActionThePlanCoveredIsAQuietLine() throws {
    #expect(ChatTimeline.kinds.contains("approval.plan"), "the apps ask for it")
    let covered = { (seq: Int, card: String) in
        try event(
            seq, "approval.plan", #"{"task":"t1","approval":"a1b2c3d4e5f6","by":"someone@example.com","card":\#(card)}"#)
    }
    let timeline = ChatTimeline(replaying: [
        try event(1, "message", #"{"role":"user","text":"Tidy up","task":"t1"}"#),
        try covered(
            2,
            #"{"kind":"app","action":"act","app":"Mousepad","window":"notes.txt - Mousepad","summary":"Add the milk line and save","steps":[{"action":"type","text":"- milk"},{"action":"key","key":"CTRL+S"}]}"#
        ),
        try covered(3, #"{"kind":"browser","action":"type","site":"app.slack.com","target":"Message","text":"Done","summary":""}"#),
        try covered(4, #"{"kind":"browser","action":"click","site":"app.slack.com","role":"button","target":"Send","summary":"Send it"}"#),
        try covered(5, #"{"kind":"shell","command":"git push -u origin fix","summary":""}"#),
        try covered(6, #"{"kind":"shell","command":"git push -u origin fix","summary":"Push the branch"}"#),
        try covered(7, #"{"kind":"install","name":"slack","app":"Slack"}"#),
        try covered(8, "null"),
    ])
    let rows = timeline.visibleItems
    #expect(rows.map(\.id) == ["user-1"] + (2...8).map { "covered-\($0)" })
    let actions = rows.compactMap { if case .covered(let action) = $0 { action } else { nil } }
    #expect(actions[0].task == "t1" && actions[0].approval == "a1b2c3d4e5f6" && actions[0].by == "someone@example.com")
    #expect(actions[0].app?.steps.map(\.action) == ["type", "key"] && actions[0].app?.steps[1].key == "CTRL+S")
    #expect(actions[0].app?.imagePath == nil && actions[1].browser?.text == "Done")
    typealias Line = CoveredAction.Line
    #expect(
        actions.map(\.line) == [
            Line(place: "Mousepad", what: "Add the milk line and save", more: true),
            Line(place: "app.slack.com", what: "Write in “Message”", more: true),
            Line(place: "app.slack.com", what: "Send it", more: true),
            Line(place: "", what: "git push -u origin fix", code: true),
            Line(place: "", what: "Push the branch", more: true),
            Line(place: "", what: "Install Slack", more: true),
            Line(place: "", what: "An action"),
        ])
}

@Test func aPlansCommandsAndSavedKeysAreChipsThatComeOff() throws {
    let (_, plan) = try planApproval(
        #"""
        {"id":"p3","task":"t1","kind":"plan","editable":true,
         "summary":"Approve a plan of 2 steps","command":"1. Push the branch\n2. Tell the user",
         "steps":[{"step":"Push the branch and open the PR","status":"pending","shell":true,"keys":["GITHUB_TOKEN","GITHUB_TOKEN",""]},
                  {"step":"Tell the user","status":"pending"}],
         "covers":{"sites":[],"apps":[],"shell":true,"keys":["GITHUB_TOKEN"],"commands":[],"installs":[]}}
        """#)
    let push = plan.steps[0]
    #expect(push == PlanStep(step: "Push the branch and open the PR", status: .pending, shell: true, keys: ["GITHUB_TOKEN"]))
    #expect(push.needs.map(\.kind) == [.shell, .key])
    #expect(plan.covers == PlanCovers(shell: true, keys: ["GITHUB_TOKEN"]))
    #expect(
        said(plan.covers)
            == "running commands that would ask (deletes, pushes, uploads) · commands using your key `GITHUB_TOKEN`")
    let noShell = push.removing(need: 0)
    #expect(noShell == PlanStep(step: push.step, status: .pending, keys: ["GITHUB_TOKEN"]))
    #expect(said(plan.covers.needed(by: [noShell, plan.steps[1]])) == "commands using your key `GITHUB_TOKEN`")
    let noKey = push.removing(need: 1)
    #expect(noKey == PlanStep(step: push.step, status: .pending, shell: true))
    #expect(said(plan.covers.needed(by: [noKey])) == "running commands that would ask (deletes, pushes, uploads)")
    #expect(plan.covers.needed(by: [noKey.removing(need: 0)]).phrases.isEmpty)
    // What goes back to agentd.
    let sent = try JSONSerialization.jsonObject(with: JSONEncoder().encode([push.edit, noKey.edit])) as? [[String: Any]]
    #expect(sent?[0]["shell"] as? Bool == true && sent?[0]["keys"] as? [String] == ["GITHUB_TOKEN"])
    #expect(sent?[1]["keys"] == nil)
}

@Test func allowForThisTaskIsOfferedWhereAGrantCoversTheCard() throws {
    var timeline = ChatTimeline()
    timeline.apply(
        try event(
            1, "approval.requested",
            #"{"id":"c1","task":"t1","kind":"shell","command":"git push","reason":"uses your saved key","secrets":["GITHUB_TOKEN"],"for_task":"commands that would ask (deletes, kills, pushes, uploads); commands using the key GITHUB_TOKEN"}"#
        ))
    timeline.apply(
        try event(2, "approval.requested", #"{"id":"c2","task":"t1","kind":"shell","command":"pip download x","reason":"r","direct":true}"#))
    timeline.apply(
        try event(3, "approval.requested", #"{"id":"c3","task":"t1","kind":"plan","steps":[{"step":"Go","status":"pending"}],"for_task":"x"}"#))
    let cards = timeline.visibleItems.compactMap { item -> Approval? in
        if case .approval(let card) = item { return card }
        return nil
    }
    #expect(cards.map(\.forTask) == [
        "commands that would ask (deletes, kills, pushes, uploads); commands using the key GITHUB_TOKEN", nil, nil,
    ])
    // The answer says so only when it's asked for.
    let encoded = { (answer: ApprovalAnswer) in String(decoding: try JSONEncoder().encode(answer), as: UTF8.self) }
    #expect(try encoded(ApprovalAnswer(approved: true, text: nil, forTask: true)).contains(#""for_task":true"#))
    #expect(try !encoded(ApprovalAnswer(approved: true, text: nil)).contains("for_task"))
}
