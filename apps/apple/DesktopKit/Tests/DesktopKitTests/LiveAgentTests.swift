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
