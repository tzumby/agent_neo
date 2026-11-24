import Foundation
import Testing

@testable import AgentAPI
@testable import ChatCore

/// Finding the card a tapped notification points to (`ChatStore.locate`).

private func event(_ seq: Int, _ kind: String, _ json: String) throws -> AgentEvent {
    AgentEvent(seq: seq, kind: kind, data: try JSONDecoder().decode(JSONValue.self, from: Data(json.utf8)))
}

private func card(_ seq: Int, _ id: String) throws -> AgentEvent {
    try event(seq, "approval.requested", #"{"task":"t1","id":"\#(id)","kind":"shell","command":"ls"}"#)
}

private func message(_ seq: Int) throws -> AgentEvent {
    try event(seq, "message", #"{"role":"user","text":"hi","task":"t1"}"#)
}

/// A chat loaded and connected, as after the app was suspended: what's loaded
/// ends at `lastSeq`.
@MainActor
private func connectedStore(_ events: [AgentEvent], more: Bool = false, lastSeq: Int) -> ChatStore {
    let store = ChatStore()
    store.show(newest: EventPage(events: events, more: more, lastSeq: lastSeq))
    store.connection = .live
    return store
}

@MainActor
@Test func aCardStillOnTheStreamIsWaitedForNotReportedGone() async throws {
    let store = connectedStore([try message(1)], more: true, lastSeq: 5)
    let search = Task {
        await store.locate(
            "approval-a1", connecting: .seconds(5), catchingUp: .seconds(5), poll: .milliseconds(5),
            newestChatSeq: { 7 },
            older: {
                Issue.record("paged back before the chat was current")
                return false
            })
    }
    try await Task.sleep(for: .milliseconds(50))
    store.receive(try card(7, "a1"))
    #expect(await search.value == .waiting)
}

@MainActor
@Test func anOlderCardIsFoundByPagingBackOnceTheChatIsCurrent() async throws {
    let store = connectedStore([try message(10)], more: true, lastSeq: 12)
    var pages = 0
    let found = await store.locate(
        "approval-a1", connecting: .seconds(5), catchingUp: .seconds(5), poll: .milliseconds(5),
        newestChatSeq: { 10 },
        older: {
            pages += 1
            store.insert(older: EventPage(events: [try! card(3, "a1")], more: false, lastSeq: 12))
            return true
        })
    #expect(found == .waiting)
    #expect(pages == 1)
}

@MainActor
@Test func anAnsweredCardIsSettledAndAnUnknownOneMissing() async throws {
    let store = connectedStore(
        [try card(3, "a1"), try event(4, "approval.resolved", #"{"id":"a1","approved":true}"#)], lastSeq: 4)
    let answered = await store.locate(
        "approval-a1", connecting: .seconds(5), catchingUp: .seconds(5), poll: .milliseconds(5),
        newestChatSeq: { 4 }, older: { false })
    #expect(answered == .settled)
    let unknown = await store.locate(
        "approval-zz", connecting: .seconds(5), catchingUp: .seconds(5), poll: .milliseconds(5),
        newestChatSeq: { 4 }, older: { false })
    #expect(unknown == .missing)
}

@MainActor
@Test func withoutTheDesktopTheSearchGivesUpQuietly() async throws {
    // Never connected.
    let offline = ChatStore()
    let never = await offline.locate(
        "approval-a1", connecting: .milliseconds(40), catchingUp: .seconds(5), poll: .milliseconds(5),
        newestChatSeq: { 0 }, older: { false })
    #expect(never == .unreachable)
    // Connected on paper, but the desktop doesn't answer.
    let stale = connectedStore([try message(1)], lastSeq: 1)
    let behind = await stale.locate(
        "approval-a1", connecting: .seconds(5), catchingUp: .milliseconds(40), poll: .milliseconds(5),
        newestChatSeq: { throw URLError(.timedOut) }, older: { false })
    #expect(behind == .unreachable)
    // Current, but an older page wouldn't load.
    let paging = connectedStore([try message(1)], more: true, lastSeq: 1)
    let failed = await paging.locate(
        "approval-a1", connecting: .seconds(5), catchingUp: .seconds(5), poll: .milliseconds(5),
        newestChatSeq: { 1 }, older: { false })
    #expect(failed == .unreachable)
}
