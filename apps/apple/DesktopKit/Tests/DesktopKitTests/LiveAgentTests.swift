import Foundation
import Testing

@testable import AgentAPI
@testable import ChatCore

/// Against a real agentd, e.g. one run locally on 127.0.0.1:8765:
///   AGENTD_URL=http://127.0.0.1:8765 swift test --filter LiveAgent
/// Skipped unless AGENTD_URL is set. Uses the model (costs a fraction of a cent).
@Test(.enabled(if: ProcessInfo.processInfo.environment["AGENTD_URL"] != nil))
func liveAgentStreamsATask() async throws {
    let url = try #require(URL(string: ProcessInfo.processInfo.environment["AGENTD_URL"] ?? ""))
    let client = AgentClient(baseURL: url, session: .shared)
    let start = try await client.status().lastSeq
    try await client.send("Reply with the single word: pong. Don't use any tools.")

    var timeline = ChatTimeline()
    for try await event in client.events(after: start) {
        timeline.apply(event)
        if event.kind == "task.finished" { break }
    }
    #expect(!timeline.busy)
    guard case .assistant(let answer) = timeline.items.last else {
        Issue.record("no answer: \(timeline.items)")
        return
    }
    #expect(answer.text.lowercased().contains("pong"))
}

/// Conversations, queued messages and usage against a real agentd 0.16.0,
/// e.g. `make web-dev` (its scripted model):
///   AGENTD_URL=http://127.0.0.1:8088 swift test --filter LiveAgent
@Test(.enabled(if: ProcessInfo.processInfo.environment["AGENTD_URL"] != nil))
func liveAgentQueuesInAConversationOfItsOwn() async throws {
    let url = try #require(URL(string: ProcessInfo.processInfo.environment["AGENTD_URL"] ?? ""))
    let client = AgentClient(baseURL: url, session: .shared)
    let status = try await client.status()
    #expect(status.sessions != nil && status.budget != nil)
    #expect(try await client.sessions().sessions.contains { $0.id == AgentSession.main })
    let usage = try await client.usage(days: 7)
    #expect(usage.daily.count == 7)

    let made = try await client.createSession()
    #expect(made.isBlank)
    #expect(try await client.updateSession(made.id, title: "Live test").title == "Live test")
    // Two at once: the second waits its turn, then Stop drops it.
    let first = try await client.send("Reply with the single word: pong.", session: made.id)
    guard case .started = first else {
        Issue.record("the first message didn't start: \(first)")
        return
    }
    let second = try await client.send("And again.", session: made.id)
    guard case .queued(let queued, let position) = second else {
        Issue.record("the second message didn't queue: \(second)")
        return
    }
    #expect(position == 1)
    #expect(try await client.session(made.id).queued.map(\.id) == [queued])
    try await client.unqueue(queued, session: made.id)
    await #expect(throws: AgentError.self) { try await client.unqueue(queued, session: made.id) }
    let page = try await client.eventPage(before: nil, limit: 20, kinds: ChatTimeline.kinds, session: made.id)
    #expect(page.events.allSatisfy { ChatStore.conversation(of: $0) ?? made.id == made.id })
    #expect(ChatTimeline(replaying: page.events).queued.isEmpty)
    try await client.stop(session: made.id)

    // Deleting waits for the task to end (409 while it works).
    for _ in 0..<50 where try await client.session(made.id).state == .working {
        try await Task.sleep(for: .milliseconds(200))
    }
    try await client.deleteSession(made.id)
    await #expect(throws: AgentError.self) { try await client.session(made.id) }
}
