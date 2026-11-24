import Foundation
import Testing

@testable import AgentAPI
@testable import ChatCore

/// Conversations (agentd 0.16.0): events routed by `session`, queued
/// messages, and a desktop from before them.

private func event(_ seq: Int, _ kind: String, _ json: String) throws -> AgentEvent {
    AgentEvent(seq: seq, kind: kind, data: try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8)))
}

@MainActor
private func users(_ chat: Conversation) -> [String] {
    chat.timeline.items.compactMap { if case .user(let message) = $0 { message.text } else { nil } }
}

// MARK: Routing

@Test func eventsBelongToTheirSessionOrToMainOrToAll() throws {
    let route = { (kind: String, json: String) in ChatStore.conversation(of: try event(1, kind, json)) }
    #expect(try route("message", #"{"role":"user","text":"hi","session":"s2"}"#) == "s2")
    #expect(try route("approval.requested", #"{"task":"t1","id":"a1","session":"s3"}"#) == "s3")
    // An older agentd sends no session: a conversation's events are main's.
    #expect(try route("message", #"{"role":"user","text":"hi"}"#) == "main")
    #expect(try route("task.stopping", "{}") == "main")
    #expect(try route("action.resolved", #"{"id":"x1","choice":"done"}"#) == "main")
    #expect(try route("message.unqueued", #"{"id":"q1","reason":"removed"}"#) == "main")
    #expect(try route("memory.saved", #"{"task":"t1","id":7}"#) == "main", "a memory saved during a task")
    // Global: every conversation applies them.
    #expect(try route("secret.saved", #"{"name":"GITHUB_TOKEN"}"#) == nil)
    #expect(try route("settings", #"{"model":"m"}"#) == nil)
    #expect(try route("budget.alert", #"{"level":80,"spent":40,"budget":50,"month":"2026-11"}"#) == nil)
    #expect(try route("session.created", #"{"id":"s4","title":""}"#) == nil)
}

@MainActor
@Test func theStreamFeedsEachConversationItsOwnEventsAndTheGlobalOnesToAll() throws {
    let store = ChatStore()
    // main is on screen; s2 was opened too.
    store.show(newest: EventPage(events: [], more: false, lastSeq: 10))
    store.conversation("s2").show(newest: EventPage(events: [], more: false, lastSeq: 10))

    store.receive(try event(11, "message", #"{"role":"user","text":"In s2","task":"t2","session":"s2"}"#))
    store.receive(try event(12, "task.started", #"{"task":"t2","session":"s2"}"#))
    store.receive(try event(13, "message", #"{"role":"user","text":"In main","task":"t1","session":"main"}"#))
    store.receive(try event(14, "message", #"{"role":"user","text":"In s3","task":"t3","session":"s3"}"#))
    // A key request waits in s2; a key saved for it is a global event.
    store.receive(
        try event(15, "approval.requested", #"{"task":"t2","id":"k1","kind":"secret","name":"API_KEY","session":"s2"}"#))
    store.receive(try event(16, "secret.saved", #"{"name":"API_KEY"}"#))

    let s2 = store.conversation("s2")
    #expect(users(s2) == ["In s2"])
    #expect(users(store.current) == ["In main"])
    #expect(s2.busy && !store.current.busy, "one conversation working doesn't make the others busy")
    guard case .approval(let card) = s2.timeline.items.last else {
        Issue.record("no key request in s2")
        return
    }
    #expect(card.secret?.saved == true)
    // s3 wasn't opened: its chat loads from its own page when it is.
    #expect(!store.conversation("s3").loaded && store.conversation("s3").timeline.items.isEmpty)
    #expect(store.lastSeq == 16)
}

@MainActor
@Test func streamEventsThatComeWhileAPageLoadsAreKeptUnlessThePageHasThem() throws {
    let store = ChatStore()
    let chat = store.conversation("s2")
    chat.loading()
    store.receive(try event(20, "message", #"{"role":"user","text":"In the page too","session":"s2"}"#))
    store.receive(try event(22, "message", #"{"role":"user","text":"After the page","session":"s2"}"#))
    #expect(chat.timeline.items.isEmpty)
    chat.show(
        newest: EventPage(
            events: [try event(20, "message", #"{"role":"user","text":"In the page too","session":"s2"}"#)],
            more: false, lastSeq: 21))
    #expect(users(chat) == ["In the page too", "After the page"])
    // The stream starting again from an earlier point doesn't repeat it.
    store.receive(try event(22, "message", #"{"role":"user","text":"After the page","session":"s2"}"#))
    #expect(users(chat).count == 2)
}

// MARK: Queued messages

@Test func aQueuedMessageIsABubbleUntilItStartsOrIsRemovedOrStopped() throws {
    var timeline = ChatTimeline()
    timeline.apply(try event(1, "message", #"{"role":"user","text":"Plan the trip","task":"t1","session":"s2"}"#))
    timeline.apply(try event(2, "task.started", #"{"task":"t1","session":"s2"}"#))
    let queue = { (seq: Int, id: String, text: String, created: Int) in
        try event(
            seq, "message.queued",
            #"{"session":"s2","id":"\#(id)","text":"\#(text)","by":"iPhone","created":\#(created),"position":1}"#)
    }
    timeline.apply(try queue(3, "q1", "Book the hotel too", 1_795_532_470))
    timeline.apply(try queue(4, "q2", "And a car", 1_795_532_471))
    #expect(timeline.queued.map(\.id) == ["q1", "q2"])
    #expect(timeline.queued.first?.text == "Book the hotel too" && timeline.queued.first?.by == "iPhone")
    #expect(timeline.queued.first?.created == Date(timeIntervalSince1970: 1_795_532_470))
    // Not part of the chat until it starts.
    #expect(timeline.visibleItems.count == 1)

    // Started: the bubble goes, the usual message follows.
    timeline.apply(try event(5, "task.finished", #"{"task":"t1","status":"done","session":"s2"}"#))
    timeline.apply(try event(6, "message.unqueued", #"{"session":"s2","id":"q1","reason":"started","task":"t2"}"#))
    timeline.apply(try event(7, "message", #"{"role":"user","text":"Book the hotel too","task":"t2","session":"s2"}"#))
    timeline.apply(try event(8, "task.started", #"{"task":"t2","session":"s2"}"#))
    #expect(timeline.queued.map(\.id) == ["q2"])
    #expect(timeline.visibleItems.last == .user(Message(id: 7, text: "Book the hotel too", task: "t2")))

    // Removed with its ✕.
    timeline.apply(try event(9, "message.unqueued", #"{"session":"s2","id":"q2","reason":"removed"}"#))
    #expect(timeline.queued.isEmpty)

    // Stop drops the rest.
    timeline.apply(try queue(10, "q3", "One", 1_795_532_480))
    timeline.apply(try queue(11, "q4", "Two", 1_795_532_481))
    timeline.apply(try event(12, "task.stopping", #"{"session":"s2"}"#))
    timeline.apply(try event(13, "message.unqueued", #"{"session":"s2","id":"q3","reason":"stopped"}"#))
    timeline.apply(try event(14, "message.unqueued", #"{"session":"s2","id":"q4","reason":"stopped"}"#))
    #expect(timeline.queued.isEmpty)
    // A repeated event doesn't bring one back.
    timeline.apply(try queue(3, "q1", "Book the hotel too", 1_795_532_470))
    #expect(timeline.queued.isEmpty)
}

@Test func theListedQueueFillsInWhatTheLoadedEventsDontHave() throws {
    var timeline = ChatTimeline()
    // The newest page has q2's queuing and q1 leaving; q1 and q3 were queued on an older page.
    timeline.apply(try event(30, "message.unqueued", #"{"session":"s2","id":"q1","reason":"started","task":"t5"}"#))
    timeline.apply(
        try event(31, "message.queued", #"{"session":"s2","id":"q2","text":"Second","created":1795532472}"#))
    let listed = [
        QueuedMessage(id: "q1", text: "First", created: Date(timeIntervalSince1970: 1_795_532_470)),  // stale
        QueuedMessage(id: "q3", text: "Earlier", created: Date(timeIntervalSince1970: 1_795_532_471)),
        QueuedMessage(id: "q2", text: "Second", created: Date(timeIntervalSince1970: 1_795_532_472)),
    ]
    #expect(timeline.queue(merging: listed).map(\.id) == ["q3", "q2"])
    #expect(timeline.queue(merging: []).map(\.id) == ["q2"])
}

@MainActor
@Test func aQueuedSendShowsAtOnceAndItsCrossHidesItAtOnce() async throws {
    let stub = StubAgentd()
    stub.on("POST /api/v1/sessions/s2/messages", status: 202, #"{"queued":{"id":"q9","position":1}}"#)
    stub.on("DELETE /api/v1/sessions/s2/queue/q9", status: 204, "")
    stub.on("POST /api/v1/sessions/s2/stop", #"{"stopped":true,"dropped":1}"#)
    let store = ChatStore()
    store.apply(SessionList(sessions: [AgentSession(id: "main", title: "First"), AgentSession(id: "s2", title: "Trip")]))
    let chat = store.conversation("s2")
    chat.show(newest: EventPage(events: [], more: false, lastSeq: 1))

    await store.send("Book the hotel too", to: "s2", client: stub.client)
    #expect(chat.queue.map(\.text) == ["Book the hotel too"])
    // Its event comes: still one bubble.
    store.receive(
        try event(2, "message.queued", #"{"session":"s2","id":"q9","text":"Book the hotel too","created":1795532470}"#))
    #expect(chat.queue.map(\.id) == ["q9"])

    await store.remove(chat.queue[0], from: "s2", client: stub.client)
    #expect(chat.queue.isEmpty)
    #expect(store.notice == nil)
    #expect(stub.requests.contains("DELETE /api/v1/sessions/s2/queue/q9"))
}

@MainActor
@Test func removingAQueuedMessageThatAlreadyStartedIsFine() async throws {
    let stub = StubAgentd()
    stub.on("DELETE /api/v1/sessions/main/queue/q1", status: 404, #"{"error":"not queued"}"#)
    let store = ChatStore()
    store.apply(SessionList(sessions: [AgentSession(id: "main", title: "First")]))
    store.show(newest: EventPage(events: [], more: false, lastSeq: 1))
    store.receive(try event(2, "message.queued", #"{"session":"main","id":"q1","text":"Hi","created":1}"#))
    await store.remove(store.current.queue[0], from: "main", client: stub.client)
    #expect(store.current.queue.isEmpty && store.notice == nil)
}

// MARK: The store against a desktop

private let status016 = """
    {"desktop":"neo-agent","busy":false,"settings":{"model":"m","allow_actions":true},"last_seq":40,
     "known_models":["m"],"shell_tool":true,"version":"0.16.0",
     "sessions":{"working":1,"waiting":0,"queued":1,"running_limit":3},
     "budget":{"monthly_usd":50.0,"spent":41.2,"fraction":0.824,"paused":false,"month":"2026-11"}}
    """

private let status015 = """
    {"desktop":"neo-agent","busy":false,"settings":{"model":"m","allow_actions":true},"last_seq":40,
     "known_models":["m"],"shell_tool":true,"version":"0.15.2"}
    """

@MainActor
@Test func anOlderDesktopIsOneConversationMain() async throws {
    let key = "test.lastConversation.\(UUID())"
    UserDefaults.standard.set("s2", forKey: key)  // from when it had conversations
    defer { UserDefaults.standard.removeObject(forKey: key) }

    let stub = StubAgentd()
    stub.on("GET /api/v1/status", status015)
    // /api/v1/sessions isn't there: 404.
    stub.on(
        "GET /api/v1/events/page",
        #"{"events":[{"seq":38,"ts":"2026-11-24T10:00:00Z","kind":"message","data":{"role":"user","text":"Hello","task":"t1"}}],"more":false,"last_seq":40}"#
    )
    stub.on("GET /api/v1/events") { _ in
        StubAgentd.Reply(
            body: sse([
                (41, "task.started", #"{"task":"t1"}"#),
                (42, "approval.requested", #"{"task":"t1","id":"a1","command":"ls","reason":"r"}"#),
                (43, "task.paused", #"{"task":"t1","approval":"a1"}"#),
            ]), stream: true)
    }
    stub.on("POST /api/v1/messages", status: 409, #"{"error":"busy"}"#)
    stub.on("POST /api/v1/stop", #"{"stopped":true}"#)

    let store = ChatStore(remembering: key)
    #expect(store.selected == "s2")
    let run = Task { await store.run(client: stub.client) }
    defer { run.cancel() }
    try await waitUntil { store.current.timeline.items.count == 2 && !store.current.busy }

    #expect(store.conversationsAvailable == false)
    #expect(store.selected == "main" && store.sessions.isEmpty)
    #expect(UserDefaults.standard.string(forKey: key) == "s2", "kept for when the desktop is updated")
    // Pages as before: no conversation asked for.
    let page = try #require(stub.requests.first { $0.hasPrefix("GET /api/v1/events/page") })
    #expect(!page.contains("session="))
    // Events without a session are main's.
    #expect(store.timeline.visibleItems.map(\.id) == ["user-38", "approval-a1"])

    // The old endpoints; its 409 is the old "busy".
    await store.send("Another", client: stub.client)
    #expect(store.notice == "Your desktop is busy with another request. Stop it first.")
    await store.stop(client: stub.client)
    #expect(stub.requests.contains("POST /api/v1/messages"))
    #expect(stub.requests.contains("POST /api/v1/stop"))
    #expect(!stub.requests.contains { $0.contains("/sessions/") })
    // Nothing to select on an older desktop.
    store.select("s3")
    #expect(store.selected == "main")
}

@MainActor
@Test func aNewDesktopOpensTheConversationUsedLastAndRoutesTheStream() async throws {
    let key = "test.lastConversation.\(UUID())"
    UserDefaults.standard.set("s2", forKey: key)
    defer { UserDefaults.standard.removeObject(forKey: key) }

    let stub = StubAgentd()
    stub.on("GET /api/v1/status", status016)
    stub.on(
        "GET /api/v1/sessions",
        """
        {"sessions":[
          {"id":"s2","title":"Plan the trip to Lisbon","created":1795530000,"updated":1795532478,"archived":false,
           "state":"working","needs_you":null,"queued":[{"id":"q1","text":"Book the hotel","by":"iPhone","created":1795532470}],
           "last":{"role":"user","text":"Plan the trip to Lisbon","ts":1795532478}},
          {"id":"main","title":"First conversation","created":1795000000,"updated":1795000100,"archived":false,
           "state":"idle","needs_you":null,"queued":[],"last":null}],
         "running_limit":3}
        """)
    stub.on("GET /api/v1/events/page") { request in
        let session = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?
            .queryItems?.first { $0.name == "session" }?.value
        let text = session == "s2" ? "Plan the trip to Lisbon" : "Hello"
        return StubAgentd.Reply(
            body:
                #"{"events":[{"seq":30,"ts":"2026-11-24T10:00:00Z","kind":"message","data":{"role":"user","text":"\#(text)","task":"t\#(session ?? "")","session":"\#(session ?? "")"}}],"more":false,"last_seq":40}"#
        )
    }
    stub.on("GET /api/v1/events") { _ in
        StubAgentd.Reply(
            body: sse([
                (41, "task.started", #"{"task":"ts2","session":"s2"}"#),
                (42, "message", #"{"role":"user","text":"Elsewhere","task":"tx","session":"s3"}"#),
                (43, "message.queued", #"{"id":"q2","text":"And a car","created":1795532480,"session":"s2"}"#),
            ]), stream: true)
    }

    let store = ChatStore(remembering: key)
    let run = Task { await store.run(client: stub.client) }
    defer { run.cancel() }
    try await waitUntil { store.current.queue.count == 2 }

    #expect(store.conversationsAvailable == true)
    #expect(store.selected == "s2")
    #expect(store.sessions.map(\.id) == ["s2", "main"])
    #expect(stub.requests.contains { $0.hasPrefix("GET /api/v1/events/page") && $0.contains("session=s2") })
    let s2 = store.current
    #expect(users(s2) == ["Plan the trip to Lisbon"] && s2.busy)
    // The queue as listed, then as its events say.
    #expect(s2.queue.map(\.id) == ["q1", "q2"])

    // The list is fetched again, once, shortly after a burst of events that change it.
    try await waitUntil { stub.count("GET /api/v1/sessions") == 2 }
    try await Task.sleep(for: .milliseconds(400))
    #expect(stub.count("GET /api/v1/sessions") == 2)

    // Opening main loads its own page.
    await store.open("main", client: stub.client)
    #expect(users(store.conversation("main")) == ["Hello"])
    #expect(stub.requests.contains { $0.hasPrefix("GET /api/v1/events/page") && $0.contains("session=main") })
    store.select("main")
    #expect(UserDefaults.standard.string(forKey: key) == "main")
}

@MainActor
@Test func aConversationDeletedElsewhereFallsBackToMain() async throws {
    let key = "test.lastConversation.\(UUID())"
    UserDefaults.standard.set("gone", forKey: key)
    defer { UserDefaults.standard.removeObject(forKey: key) }
    let stub = StubAgentd()
    stub.on("GET /api/v1/sessions", #"{"sessions":[{"id":"main","title":"First"}],"running_limit":3}"#)
    stub.on("GET /api/v1/sessions/gone", status: 404, #"{"error":"no such conversation"}"#)
    stub.on("GET /api/v1/sessions/old", #"{"id":"old","title":"Archived one","archived":true}"#)
    let store = ChatStore(remembering: key)
    try await store.refreshList(client: stub.client)
    #expect(store.selected == "main")
    // An archived one isn't listed, but it's still there.
    store.select("old")
    try await store.refreshList(client: stub.client)
    #expect(store.selected == "old")
}

@MainActor
@Test func aNewConversationReusesAnEmptyOne() async throws {
    let stub = StubAgentd()
    stub.on("POST /api/v1/sessions", status: 201, #"{"id":"s9","title":"","created":1795532500,"updated":1795532500}"#)
    let store = ChatStore()
    store.apply(SessionList(sessions: [AgentSession(id: "main", title: "First conversation")]))
    #expect(await store.newConversation(client: stub.client) == "s9")
    #expect(store.selected == "s9" && store.sessions.first?.id == "s9")
    // Again, while s9 is still empty: no second one.
    store.select("main")
    #expect(await store.newConversation(client: stub.client) == "s9")
    #expect(stub.count("POST /api/v1/sessions") == 1)
}
