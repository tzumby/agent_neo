import AgentAPI
import Foundation
import Observation

/// Keeps one desktop's chat in sync: loads the newest page of the chat, older
/// pages as the user scrolls up, and streams agentd's events after that
/// (reconnecting and resuming after the last one seen). Sends the user's
/// commands.
@MainActor
@Observable
public final class ChatStore {
    public enum Connection: Equatable, Sendable {
        case connecting
        case live
        case offline(String)
    }

    public private(set) var timeline = ChatTimeline()
    public private(set) var connection: Connection = .connecting
    public private(set) var status: AgentStatus?
    /// The newest page of the chat is in: the chat can show, at its end.
    public private(set) var loaded = false
    /// Older chat is on the desktop, not loaded yet.
    public private(set) var hasOlder = false
    /// A one-off problem to show the user (busy, network, …).
    public var notice: String?

    /// The chat's events loaded so far, oldest first (the timeline is built from them).
    private var history: [AgentEvent] = []
    private var lastSeq = 0
    private var olderPage: Task<Bool, Never>?
    /// Cards the desktop no longer knows (answered 404), kept across rebuilds.
    private var expiredApprovals: Set<String> = []
    private var expiredActions: Set<String> = []

    /// Events per page: a dozen or so exchanges.
    static let pageSize = 60

    public init() {}

    /// Events after which the status (its stage above all) is worth fetching again.
    static let changesStatus: Set<String> = [
        "settings", "task.started", "task.paused", "task.resumed", "task.finished", "approval.requested",
        "approval.resolved", "action_needed", "action.resolved", "secret.saved", "memory.pending",
        "memory.confirmed", "memory.forgotten", "schedule.created", "schedule.updated", "schedule.deleted",
    ]

    /// What the live stream asks for: the chat, and what changes the status.
    static let streamed = ChatTimeline.kinds.union(changesStatus)

    public var busy: Bool { timeline.busy }
    public var stopping: Bool { timeline.stopping }
    /// The seq of the oldest event loaded: changes with each older page.
    public var oldestLoaded: Int? { history.first?.seq }

    /// Runs until the calling task is cancelled (e.g. the view disappears or
    /// the Tailscale session is replaced).
    public func run(client: AgentClient) async {
        while !Task.isCancelled {
            connection = .connecting
            do {
                status = try await client.status()
                // The desktop answered; a quiet chat may not send an event for a while.
                connection = .live
                if !loaded { try await loadNewest(client: client) }
                for try await event in client.events(after: lastSeq, kinds: Self.streamed) {
                    guard event.seq > lastSeq else { continue }
                    lastSeq = event.seq
                    if ChatTimeline.kinds.contains(event.kind) {
                        history.append(event)
                        timeline.apply(event)
                    }
                    if Self.changesStatus.contains(event.kind) {
                        status = try? await client.status()
                    }
                }
            } catch {
                if Task.isCancelled { break }
                connection = .offline(error.localizedDescription)
            }
            try? await Task.sleep(for: .seconds(2))
        }
    }

    /// The newest page, in one go: the chat appears at its end rather than
    /// filling in event by event.
    private func loadNewest(client: AgentClient) async throws {
        do {
            let page = try await client.eventPage(before: nil, limit: Self.pageSize, kinds: ChatTimeline.kinds)
            history = page.events
            hasOlder = page.more
            lastSeq = page.lastSeq
            rebuild()
        } catch let error as AgentError where error.statusCode == 404 {
            // agentd before 0.15.1 has no pages: the stream replays the whole log.
        }
        loaded = true
    }

    /// The page before the oldest one loaded, put in front of it. Returns
    /// whether one came. A load already under way is joined, not repeated.
    @discardableResult
    public func loadOlder(client: AgentClient) async -> Bool {
        if let olderPage { return await olderPage.value }
        guard hasOlder, let before = history.first?.seq else { return false }
        // Not tied to the caller: a page asked for by a row that scrolls away still lands.
        let load = Task {
            defer { olderPage = nil }
            guard
                let page = try? await client.eventPage(
                    before: before, limit: Self.pageSize, kinds: ChatTimeline.kinds)
            else { return false }
            history.insert(contentsOf: page.events, at: 0)
            hasOlder = page.more
            rebuild()
            return true
        }
        olderPage = load
        return await load.value
    }

    /// The timeline again from all the events loaded (an answer in a newer
    /// page closes a card from an older one).
    private func rebuild() {
        var rebuilt = ChatTimeline(replaying: history)
        expiredApprovals.forEach { rebuilt.expire(approval: $0) }
        expiredActions.forEach { rebuilt.expire(action: $0) }
        if timeline.stopping { rebuilt.markStopping() }
        timeline = rebuilt
    }

    public func send(_ text: String, client: AgentClient) async {
        await attempt { try await client.send(text) }
    }

    public func stop(client: AgentClient) async {
        // Show it at once; agentd confirms with task.stopping, then task.finished.
        timeline.markStopping()
        await attempt { try await client.stop() }
    }

    public func answer(_ approval: Approval, approved: Bool, client: AgentClient) async {
        await attempt(onGone: {
            $0.expiredApprovals.insert(approval.id)
            $0.timeline.expire(approval: approval.id)
        }) {
            try await client.answer(approval: approval.id, approved: approved)
        }
    }

    public func resolve(_ action: Action, done: Bool, client: AgentClient) async {
        await attempt(onGone: {
            $0.expiredActions.insert(action.id)
            $0.timeline.expire(action: action.id)
        }) {
            try await client.resolve(action: action.id, done: done)
        }
    }

    /// Saves the key a request card asked for; the waiting task carries on.
    /// Returns whether it was saved.
    @discardableResult
    public func save(secret value: String, for approval: Approval, client: AgentClient) async -> Bool {
        guard let request = approval.secret else { return false }
        do {
            let saved = try await client.saveSecret(
                name: request.name, value: value, label: request.label, approval: approval.id)
            if saved.busy {
                notice = "Saved. Your desktop is busy with another request; tap Continue when it's done."
            }
            return true
        } catch let error as AgentError where error.statusCode == 400 {
            if case .http(_, let message) = error { notice = "Couldn't save it: \(message)" }
            return false
        } catch {
            notice = "Couldn't save it: \(error.localizedDescription)"
            return false
        }
    }

    public func update(settings: AgentSettings, client: AgentClient) async {
        await attempt { _ = try await client.update(settings: settings) }
        status = try? await client.status()
    }

    private func attempt(onGone: ((ChatStore) -> Void)? = nil, _ work: () async throws -> Void) async {
        do {
            try await work()
        } catch let error as AgentError where error.statusCode == 409 {
            notice = "Your desktop is busy with another request. Stop it first."
        } catch let error as AgentError where error.statusCode == 404 && onGone != nil {
            onGone?(self)
            notice = "That request has expired."
        } catch {
            notice = error.localizedDescription
        }
    }
}
