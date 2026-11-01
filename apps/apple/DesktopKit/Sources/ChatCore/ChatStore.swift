import AgentAPI
import Foundation
import Observation

/// Keeps one desktop's chat in sync: streams agentd's events (reconnecting and
/// resuming after the last one seen) and sends the user's commands.
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
    /// A one-off problem to show the user (busy, network, …).
    public var notice: String?

    private var lastSeq = 0

    public init() {}

    /// Events after which the status (its stage above all) is worth fetching again.
    static let changesStatus: Set<String> = [
        "settings", "task.started", "task.paused", "task.resumed", "task.finished", "approval.requested",
        "approval.resolved", "action_needed", "action.resolved", "secret.saved", "memory.pending",
        "memory.confirmed", "memory.forgotten", "schedule.created", "schedule.updated", "schedule.deleted",
    ]

    public var busy: Bool { timeline.busy }
    public var stopping: Bool { timeline.stopping }

    /// Runs until the calling task is cancelled (e.g. the view disappears or
    /// the Tailscale session is replaced).
    public func run(client: AgentClient) async {
        while !Task.isCancelled {
            connection = .connecting
            do {
                status = try await client.status()
                // The desktop answered; a quiet chat may not send an event for a while.
                connection = .live
                for try await event in client.events(after: lastSeq) {
                    guard event.seq > lastSeq else { continue }
                    lastSeq = event.seq
                    timeline.apply(event)
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

    public func send(_ text: String, client: AgentClient) async {
        await attempt { try await client.send(text) }
    }

    public func stop(client: AgentClient) async {
        // Show it at once; agentd confirms with task.stopping, then task.finished.
        timeline.markStopping()
        await attempt { try await client.stop() }
    }

    public func answer(_ approval: Approval, approved: Bool, client: AgentClient) async {
        await attempt(onGone: { $0.timeline.expire(approval: approval.id) }) {
            try await client.answer(approval: approval.id, approved: approved)
        }
    }

    public func resolve(_ action: Action, done: Bool, client: AgentClient) async {
        await attempt(onGone: { $0.timeline.expire(action: action.id) }) {
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
