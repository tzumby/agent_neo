import AgentAPI
import Foundation

/// The chat as the apps show it, built from agentd's event log. A pure value:
/// apply events in order (replaying from 0 rebuilds the whole history).
public struct ChatTimeline: Sendable, Equatable {
    public private(set) var items: [ChatItem] = []
    /// A task is running (between task.started and task.finished).
    public private(set) var busy = false

    private var workIndex: [String: Int] = [:]  // task → index of its work item
    private var approvalIndex: [String: Int] = [:]
    private var actionIndex: [String: Int] = [:]

    public init() {}

    public mutating func apply(_ event: AgentEvent) {
        let data = event.data
        let task = data["task"]?.string ?? ""
        switch event.kind {
        case "message":
            let text = data["text"]?.string ?? ""
            if data["role"]?.string == "user" {
                items.append(.user(Message(id: event.seq, text: text)))
            } else {
                items.append(.assistant(Message(id: event.seq, text: text)))
            }
        case "task.started":
            busy = true
            workIndex[task] = items.count
            items.append(.work(Work(id: event.seq, task: task, model: data["model"]?.string ?? "")))
        case "step":
            let tool = data["tool"]?.string ?? "?"
            guard tool != "run_shell" else { return }  // shown by the "shell" event instead
            updateWork(task) { $0.steps.append(Work.Step(id: event.seq, tool: tool, detail: data["args"]?.string ?? "")) }
        case "shell":
            let step = Work.Step(
                id: event.seq, tool: "run_shell", detail: data["command"]?.string ?? "",
                asRoot: data["as_root"]?.bool ?? false)
            updateWork(task) { $0.steps.append(step) }
        case "approval.requested":
            let id = data["id"]?.string ?? ""
            approvalIndex[id] = items.count
            items.append(
                .approval(
                    Approval(
                        id: id, seq: event.seq, command: data["command"]?.string ?? "",
                        reason: data["reason"]?.string ?? "")))
        case "approval.resolved":
            let id = data["id"]?.string ?? ""
            let approved = data["approved"]?.bool ?? false
            update(approval: id) { $0.state = approved ? .approved : .denied }
        case "action_needed":
            let id = data["id"]?.string ?? ""
            actionIndex[id] = items.count
            items.append(
                .action(
                    Action(
                        id: id, seq: event.seq, reason: data["reason"]?.string ?? "",
                        page: data["page"]?.string ?? "", imagePath: data["image"]?.string)))
        case "action.resolved":
            let id = data["id"]?.string ?? ""
            let done = data["choice"]?.string == "done"
            update(action: id) { $0.state = done ? .done : .skipped }
        case "image":
            if let path = data["url"]?.string {
                items.append(.image(Picture(id: event.seq, path: path, caption: data["caption"]?.string)))
            }
        case "task.finished":
            busy = false
            let usage = data["usage"]
            updateWork(task) {
                $0.finished = Work.Finished(
                    status: data["status"]?.string ?? "done",
                    steps: data["steps"]?.int ?? $0.steps.count,
                    seconds: data["seconds"]?.double ?? 0,
                    cost: data["cost"]?.double,
                    inputTokens: usage?["input"]?.int ?? 0,
                    outputTokens: usage?["output"]?.int ?? 0)
            }
        default:
            break  // settings, action.resolved for unknown ids, future kinds
        }
    }

    /// The desktop no longer knows this action (agentd restarted): stop offering buttons.
    public mutating func expire(action id: String) {
        update(action: id) { $0.state = .expired }
    }

    public mutating func expire(approval id: String) {
        update(approval: id) { $0.state = .expired }
    }

    private mutating func updateWork(_ task: String, _ change: (inout Work) -> Void) {
        guard let index = workIndex[task], case .work(var work) = items[index] else { return }
        change(&work)
        items[index] = .work(work)
    }

    private mutating func update(approval id: String, _ change: (inout Approval) -> Void) {
        guard let index = approvalIndex[id], case .approval(var approval) = items[index] else { return }
        change(&approval)
        items[index] = .approval(approval)
    }

    private mutating func update(action id: String, _ change: (inout Action) -> Void) {
        guard let index = actionIndex[id], case .action(var action) = items[index] else { return }
        change(&action)
        items[index] = .action(action)
    }
}

public enum ChatItem: Sendable, Equatable, Identifiable {
    case user(Message)
    case assistant(Message)
    case work(Work)
    case approval(Approval)
    case action(Action)
    case image(Picture)

    public var id: String {
        switch self {
        case .user(let m): "user-\(m.id)"
        case .assistant(let m): "assistant-\(m.id)"
        case .work(let w): "work-\(w.id)"
        case .approval(let a): "approval-\(a.id)"
        case .action(let a): "action-\(a.id)"
        case .image(let p): "image-\(p.id)"
        }
    }
}

public struct Message: Sendable, Equatable {
    public let id: Int
    public let text: String
}

/// One task's tool calls, collapsed under a single row.
public struct Work: Sendable, Equatable {
    public struct Step: Sendable, Equatable, Identifiable {
        public let id: Int
        public let tool: String
        public let detail: String
        public var asRoot = false
    }

    public struct Finished: Sendable, Equatable {
        public let status: String
        public let steps: Int
        public let seconds: Double
        public let cost: Double?
        public let inputTokens: Int
        public let outputTokens: Int
    }

    public let id: Int
    public let task: String
    public let model: String
    public var steps: [Step] = []
    public var finished: Finished?
}

public struct Approval: Sendable, Equatable {
    public enum State: Sendable, Equatable { case pending, approved, denied, expired }
    public let id: String
    public let seq: Int
    public let command: String
    public let reason: String
    public var state: State = .pending
}

/// "Action needed": the agent stopped for something only the user can do.
public struct Action: Sendable, Equatable {
    public enum State: Sendable, Equatable { case pending, done, skipped, expired }
    public let id: String
    public let seq: Int
    public let reason: String
    /// "Title | URL" of the page the agent left open.
    public let page: String
    public let imagePath: String?
    public var state: State = .pending
}

public struct Picture: Sendable, Equatable {
    public let id: Int
    public let path: String
    public let caption: String?
}
