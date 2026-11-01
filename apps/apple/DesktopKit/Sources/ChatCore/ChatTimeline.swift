import AgentAPI
import Foundation

/// The chat as the apps show it, built from agentd's event log. A pure value:
/// apply events in order (replaying from 0 rebuilds the whole history).
public struct ChatTimeline: Sendable, Equatable {
    public private(set) var items: [ChatItem] = []
    /// A task is running (between task.started and task.finished, except while
    /// it's paused waiting for an approval).
    public private(set) var busy = false
    /// The user pressed Stop and the task hasn't ended yet.
    public private(set) var stopping = false

    private var workIndex: [String: Int] = [:]  // task → index of its work item
    private var approvalIndex: [String: Int] = [:]
    private var actionIndex: [String: Int] = [:]
    private var scheduleIndex: [Int: Int] = [:]  // schedule id → index of its note
    private var memoryIndex: [Int: Int] = [:]  // memory id → index of its note
    private var openRun: [String: Int] = [:]  // task → index of its command step still waiting for output

    public init() {}

    /// What the chat shows: everything but command approvals already answered
    /// (their outcome is on the command's step).
    public var visibleItems: [ChatItem] {
        items.filter {
            if case .approval(let approval) = $0, approval.browser == nil, approval.secret == nil,
                approval.state != .pending
            {
                return false
            }
            return true
        }
    }

    /// Stop was pressed: show it at once, before agentd's events arrive.
    public mutating func markStopping() {
        if busy { stopping = true }
    }

    public mutating func apply(_ event: AgentEvent) {
        let data = event.data
        let task = data["task"]?.string ?? ""
        switch event.kind {
        case "message":
            let text = data["text"]?.string ?? ""
            if data["role"]?.string == "user" {
                // A scheduled run's prompt, started by the desktop on its own.
                let schedule = data["schedule"]?["name"]?.string
                items.append(.user(Message(id: event.seq, text: text, schedule: schedule)))
            } else {
                // A scheduled run that found nothing: shown collapsed.
                items.append(.assistant(Message(id: event.seq, text: text, quiet: data["quiet"]?.bool ?? false)))
            }
        case "task.started":
            busy = true
            stopping = false
            workIndex[task] = items.count
            items.append(.work(Work(id: event.seq, task: task, model: data["model"]?.string ?? "")))
        case "task.paused":  // parked on an approval: the desktop is free meanwhile
            busy = false
            stopping = false
            updateWork(task) { $0.waiting = true }
        case "task.resumed":
            busy = true
            updateWork(task) { $0.waiting = false }
        case "task.stopping":
            stopping = busy
        case "step":
            let tool = data["tool"]?.string ?? "?"
            guard tool != "run_shell" else { return }  // shown by the "shell" event instead
            updateWork(task) { $0.steps.append(Work.Step(id: event.seq, tool: tool, detail: data["args"]?.string ?? "")) }
        case "shell":
            // Every command is a step in the task's row; one that needs an answer
            // also gets an approval card until it's answered.
            var step = Work.Step(
                id: event.seq, tool: "run_shell", detail: data["command"]?.string ?? "",
                asRoot: data["as_root"]?.bool ?? false)
            step.secrets = data["secrets"]?.array?.compactMap(\.string) ?? []
            step.run = ShellRun(state: data["needs_approval"]?.bool == true ? .waitingForApproval : .running)
            if let index = workIndex[task], case .work(let work) = items[index] {
                openRun[task] = work.steps.count
            }
            updateWork(task) { $0.steps.append(step) }
        case "shell.output":
            closeRun(task) {
                $0.state = data["stopped"]?.bool == true ? .didNotFinish : .finished
                $0.exitCode = data["exit_code"]?.int
                $0.timedOut = data["timed_out"]?.bool ?? false
                $0.output = data["output"]?.string ?? ""
                $0.seconds = data["seconds"]?.double
            }
        case "approval.requested":
            let id = data["id"]?.string ?? ""
            var approval = Approval(
                id: id, seq: event.seq, command: data["command"]?.string ?? "", reason: data["reason"]?.string ?? "")
            if data["kind"]?.string == "secret" {
                approval.secret = SecretRequest(
                    name: data["name"]?.string ?? "", label: data["label"]?.string ?? "",
                    why: data["why"]?.string ?? "", how: data["how"]?.string ?? "",
                    url: data["url"]?.string.flatMap(URL.init(string:)), host: data["host"]?.string,
                    replace: data["replace"]?.bool ?? false, tainted: data["tainted"]?.bool ?? false)
            } else if data["kind"]?.string == "schedule" {
                approval.schedule = ScheduleProposal(
                    name: data["name"]?.string ?? "", prompt: data["text"]?.string ?? "",
                    when: data["when"]?.string ?? "")
            } else if data["kind"]?.string == "browser" {
                approval.browser = BrowserAction(
                    action: data["action"]?.string ?? "", target: data["target"]?.string ?? "",
                    text: data["text"]?.string, key: data["key"]?.string, submit: data["submit"]?.bool ?? false,
                    sends: data["sends"]?.bool ?? false, site: data["site"]?.string ?? "")
            }
            approvalIndex[id] = items.count
            items.append(.approval(approval))
        case "approval.resolved":
            let id = data["id"]?.string ?? ""
            let approved = data["approved"]?.bool ?? false
            let isCommand = approvalIndex[id].map { index -> Bool in
                if case .approval(let approval) = items[index] { return approval.isCommand }
                return false
            } ?? false
            if data["expired"]?.bool == true {
                // Given up (stopped, a new message, or the rules changed):
                // task.finished closes its step.
                update(approval: id) {
                    $0.state = .expired
                    $0.expiredBecause = data["reason"]?.string
                }
                return
            }
            update(approval: id) { $0.state = approved ? .approved : .denied }
            if isCommand {
                if approved {
                    updateRun(task) { $0.state = .running }
                } else {
                    closeRun(task) { $0.state = .denied }
                }
            }
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
        case "schedule.created":
            guard let id = data["id"]?.int else { return }
            scheduleIndex[id] = items.count
            items.append(
                .schedule(
                    ScheduleNote(
                        id: id, seq: event.seq, name: data["name"]?.string ?? "",
                        when: data["description"]?.string ?? "", prompt: data["prompt"]?.string ?? "",
                        nextRun: data["next_run"]?.double.map(Date.init(timeIntervalSince1970:)))))
        case "schedule.deleted":
            // Cancelled, or a one-time run that has run ("done").
            guard let id = data["id"]?.int, data["done"]?.bool != true else { return }
            update(schedule: id) { $0.cancelled = true }
        case "memory.saved", "memory.pending":
            // Saves the agent made in a task; edits in Settings carry no task.
            guard data["task"]?.string != nil, let id = data["id"]?.int else { return }
            let pending = event.kind == "memory.pending"
            memoryIndex[id] = items.count
            items.append(
                .memory(
                    MemoryNote(
                        id: id, seq: event.seq, text: data["text"]?.string ?? "",
                        kind: data["kind"]?.string ?? "fact", pending: pending, reason: data["reason"]?.string,
                        inferred: !pending
                            && (data["trust"]?.string.map { $0 == "agent_inferred" } ?? (data["trusted"]?.bool == false)))))
        case "memory.confirmed":
            guard let id = data["id"]?.int else { return }
            update(memory: id) { $0.state = .approved }
        case "memory.forgotten":
            guard let id = data["id"]?.int else { return }
            update(memory: id) { $0.state = data["rejected"]?.bool == true ? .rejected : .forgotten }
        case "secret.saved":
            // Saved for a request that's still waiting: the desktop was busy.
            let name = data["name"]?.string ?? ""
            for index in items.indices {
                guard case .approval(var approval) = items[index], approval.state == .pending,
                    approval.secret?.name == name
                else { continue }
                approval.secret?.saved = true
                items[index] = .approval(approval)
            }
        case "task.finished":
            busy = false
            stopping = false
            let cancelled = data["status"]?.string == "cancelled"
            closeRun(task) { $0.state = cancelled ? .didNotFinish : .unrecorded }
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

    /// Changes the task's open command step.
    private mutating func updateRun(_ task: String, _ change: (inout ShellRun) -> Void) {
        guard let step = openRun[task] else { return }
        updateWork(task) { work in
            guard work.steps.indices.contains(step), var run = work.steps[step].run else { return }
            change(&run)
            work.steps[step].run = run
        }
    }

    /// Changes the task's open command step for the last time.
    private mutating func closeRun(_ task: String, _ change: (inout ShellRun) -> Void) {
        updateRun(task, change)
        openRun[task] = nil
    }

    private mutating func update(approval id: String, _ change: (inout Approval) -> Void) {
        guard let index = approvalIndex[id] else { return }
        guard case .approval(var approval) = items[index] else { return }
        change(&approval)
        items[index] = .approval(approval)
    }

    private mutating func update(schedule id: Int, _ change: (inout ScheduleNote) -> Void) {
        guard let index = scheduleIndex[id], case .schedule(var note) = items[index] else { return }
        change(&note)
        items[index] = .schedule(note)
    }

    private mutating func update(memory id: Int, _ change: (inout MemoryNote) -> Void) {
        guard let index = memoryIndex[id], case .memory(var note) = items[index] else { return }
        change(&note)
        items[index] = .memory(note)
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
    case schedule(ScheduleNote)
    case memory(MemoryNote)

    public var id: String {
        switch self {
        case .user(let m): "user-\(m.id)"
        case .assistant(let m): "assistant-\(m.id)"
        case .work(let w): "work-\(w.id)"
        case .approval(let a): "approval-\(a.id)"
        case .action(let a): "action-\(a.id)"
        case .image(let p): "image-\(p.id)"
        case .schedule(let s): "schedule-\(s.seq)"
        case .memory(let m): "memory-\(m.seq)"
        }
    }
}

public struct Message: Sendable, Equatable {
    public let id: Int
    public let text: String
    /// For a scheduled run's prompt: the schedule's name.
    public var schedule: String? = nil
    /// A scheduled run's answer when it found nothing new: shown collapsed.
    public var quiet = false
}

/// One task's tool calls, collapsed under a single row.
public struct Work: Sendable, Equatable {
    public struct Step: Sendable, Equatable, Identifiable {
        public let id: Int
        public let tool: String
        public let detail: String
        public var asRoot = false
        /// Saved keys the command got (by name).
        public var secrets: [String] = []
        /// For a command: how it went, shown inside the step.
        public var run: ShellRun?
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
    /// Paused, waiting for the user's approval.
    public var waiting = false
}

/// A command the agent ran, shown inside its step: how it ended and the end of
/// its output.
public struct ShellRun: Sendable, Equatable {
    public enum State: Sendable, Equatable {
        case waitingForApproval, running, finished, denied
        /// The task was stopped before it finished.
        case didNotFinish
        /// The task ended without an output event (agentd before 0.2.0).
        case unrecorded
    }

    public var state: State = .running
    public var exitCode: Int?
    public var timedOut = false
    /// The end of what it printed (stdout, then stderr).
    public var output = ""
    public var seconds: Double?
}

public struct Approval: Sendable, Equatable {
    public enum State: Sendable, Equatable { case pending, approved, denied, expired }
    public let id: String
    public let seq: Int
    public let command: String
    public let reason: String
    public var state: State = .pending
    /// Why it expired, when agentd said: `policy_changed` (the rules changed
    /// while it waited).
    public var expiredBecause: String?
    /// Set when it's a browser action (typing, a key, a click) rather than a command.
    public var browser: BrowserAction?
    /// Set when it's a schedule the agent wants to set up.
    public var schedule: ScheduleProposal?
    /// Set when the agent asks for a key or token.
    public var secret: SecretRequest?

    /// A shell command (shown in its step once answered).
    public var isCommand: Bool { browser == nil && schedule == nil && secret == nil }
}

/// The agent asks for a key or token (a GitHub token, an API key): the user
/// pastes it into a secure field, it's saved on the desktop, and the agent
/// carries on knowing only its name.
public struct SecretRequest: Sendable, Equatable {
    /// The environment variable it becomes, e.g. GITHUB_TOKEN.
    public let name: String
    /// "GitHub token".
    public let label: String
    /// What the agent needs it for.
    public let why: String
    /// Markdown steps for creating it.
    public let how: String
    /// Where it's created, e.g. GitHub's new-token page.
    public let url: URL?
    /// That link's site, shown so a look-alike stands out.
    public let host: String?
    /// A new value for a key that's saved already (expired, rejected).
    public let replace: Bool
    /// Asked for after the agent read outside content.
    public let tainted: Bool
    /// Saved, but the task hasn't carried on yet (the desktop was busy).
    public var saved = false
}

/// A schedule waiting for the user's OK: asked for after the agent read
/// outside content, so it could come from that content.
public struct ScheduleProposal: Sendable, Equatable {
    public let name: String
    public let prompt: String
    /// "Weekdays at 08:00", or "Once, Mon Nov 2 at 09:00".
    public let when: String
}

/// "Scheduled": the agent set up a task to run later or on a repeat.
public struct ScheduleNote: Sendable, Equatable {
    public let id: Int
    public let seq: Int
    public let name: String
    public let when: String
    public let prompt: String
    public let nextRun: Date?
    /// Cancelled since (in the app or by the agent).
    public var cancelled = false
}

/// The agent saved something to its memory, or proposed it for approval.
public struct MemoryNote: Sendable, Equatable {
    public enum State: Sendable, Equatable { case saved, approved, rejected, forgotten }
    public let id: Int
    public let seq: Int
    public let text: String
    /// rule, preference or fact.
    public let kind: String
    /// Waits for the user's approval in Settings → Memory.
    public let pending: Bool
    /// Why it waits: tainted_rule (saved after outside content) or conflict.
    public let reason: String?
    /// Saved after outside content, as a hint.
    public let inferred: Bool
    public var state: State = .saved
}

/// A change the agent wants to make in the desktop's browser.
public struct BrowserAction: Sendable, Equatable {
    /// "type", "key" or "click".
    public let action: String
    /// The field, button or focused element (its accessible name).
    public let target: String
    /// What it will type.
    public let text: String?
    public let key: String?
    /// Presses Enter after typing.
    public let submit: Bool
    /// Typed into a message field: approving it also lets the agent send it.
    public let sends: Bool
    /// The page's host, e.g. app.slack.com.
    public let site: String
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
