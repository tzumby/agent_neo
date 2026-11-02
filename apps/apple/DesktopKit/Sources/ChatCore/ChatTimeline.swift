import AgentAPI
import Foundation

/// The chat as the apps show it, built from agentd's event log: the user's
/// messages, the agent's answers and pictures, scheduled runs as reports, and
/// the cards waiting for the user. What the agent does on the way (steps,
/// commands, saves) stays out;
/// `busy` only says that it's working. A pure value: apply events in order.
/// Replaying the later part of the log gives the chat of that part, so the
/// apps can load it a page at a time from the newest.
public struct ChatTimeline: Sendable, Equatable {
    /// The event kinds it reads: the apps ask agentd for these only.
    public static let kinds: Set<String> = [
        "message", "image", "task.started", "task.paused", "task.resumed", "task.stopping", "task.finished",
        "approval.requested", "approval.resolved", "action_needed", "action.resolved", "secret.saved",
    ]

    public private(set) var items: [ChatItem] = []
    /// A task is running (between task.started and task.finished, except while
    /// it's paused waiting for an approval).
    public private(set) var busy = false
    /// The user pressed Stop and the task hasn't ended yet.
    public private(set) var stopping = false

    private var approvalIndex: [String: Int] = [:]
    private var actionIndex: [String: Int] = [:]
    /// A scheduled run's report, by task: its answer goes into it.
    private var reportIndex: [String: Int] = [:]

    public init() {}

    /// The chat from these events, oldest first.
    public init(replaying events: [AgentEvent]) {
        events.forEach { apply($0) }
    }

    /// What the chat shows: messages, pictures, reports, and the cards still
    /// waiting for an answer (answered, they leave the chat). Runs of one
    /// schedule that found nothing, one after another, fold into the latest
    /// (an hourly check doesn't fill the chat); the row keeps the first one's id.
    public var visibleItems: [ChatItem] {
        var shown: [ChatItem] = []
        for item in items {
            switch item {
            case .approval(let approval) where approval.state != .pending: continue
            case .action(let action) where action.state != .pending: continue
            case .report(var report) where report.quiet:
                if case .report(let previous) = shown.last, previous.quiet, previous.schedule == report.schedule {
                    report.earlier = previous.earlier + [previous.alone]
                    report.groupID = previous.groupID ?? previous.id
                    shown[shown.count - 1] = .report(report)
                } else {
                    shown.append(item)
                }
            default: shown.append(item)
            }
        }
        return shown
    }

    /// The row that shows where a task started: its message, or its report
    /// (maybe folded into a later one). For "Open that conversation".
    public func itemID(startingTask task: String) -> String? {
        visibleItems.first { item in
            switch item {
            case .user(let message): message.task == task
            case .report(let report): report.task == task || report.earlier.contains { $0.task == task }
            default: false
            }
        }?.id
    }

    /// Stop was pressed: show it at once, before agentd's events arrive.
    public mutating func markStopping() {
        if busy { stopping = true }
    }

    public mutating func apply(_ event: AgentEvent) {
        let data = event.data
        switch event.kind {
        case "message":
            let text = data["text"]?.string ?? ""
            let task = data["task"]?.string
            if data["role"]?.string == "user" {
                if let schedule = data["schedule"]?["name"]?.string {
                    // A scheduled run, started by the desktop on its own: a
                    // report, its prompt folded away as the instructions.
                    if let task { reportIndex[task] = items.count }
                    items.append(
                        .report(Report(id: event.seq, task: task, schedule: schedule, instructions: text, ts: event.ts)))
                } else {
                    items.append(.user(Message(id: event.seq, text: text, task: task)))
                }
            } else {
                // A scheduled run that found nothing: one quiet line, when it
                // says so in a line. A report is shown as one even when
                // nothing in it needed the user (no notification came).
                let quiet = data["quiet"]?.bool == true && Message.isOneLine(text)
                let message = Message(id: event.seq, text: text, task: task, quiet: quiet)
                if let task, let index = reportIndex[task], case .report(var report) = items[index], report.answer == nil {
                    report.answer = message
                    items[index] = .report(report)
                } else {
                    // An answer (or one whose report is on an older page).
                    items.append(.assistant(message))
                }
            }
        case "task.started":
            busy = true
            stopping = false
        case "task.paused":  // parked on an approval: the desktop is free meanwhile
            busy = false
            stopping = false
        case "task.resumed":
            busy = true
        case "task.stopping":
            stopping = busy
        case "task.finished":
            busy = false
            stopping = false
        case "approval.requested":
            let id = data["id"]?.string ?? ""
            var approval = Approval(
                id: id, seq: event.seq, command: data["command"]?.string ?? "", reason: data["reason"]?.string ?? "")
            approval.summary = data["summary"]?.string.flatMap { $0.isEmpty ? nil : $0 }
            approval.tainted = data["tainted"]?.bool ?? false
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
            // Expired: given up (stopped, a new message, or the rules changed).
            let state: Approval.State =
                data["expired"]?.bool == true ? .expired : data["approved"]?.bool == true ? .approved : .denied
            update(approval: data["id"]?.string ?? "") { $0.state = state }
        case "action_needed":
            let id = data["id"]?.string ?? ""
            actionIndex[id] = items.count
            items.append(
                .action(
                    Action(
                        id: id, seq: event.seq, reason: data["reason"]?.string ?? "",
                        page: data["page"]?.string ?? "", imagePath: data["image"]?.string)))
        case "action.resolved":
            let done = data["choice"]?.string == "done"
            update(action: data["id"]?.string ?? "") { $0.state = done ? .done : .skipped }
        case "image":
            if let path = data["url"]?.string {
                items.append(.image(Picture(id: event.seq, path: path, caption: data["caption"]?.string)))
            }
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
        default:
            break  // the agent's steps, commands, saves; settings; future kinds
        }
    }

    /// The desktop no longer knows this action (agentd restarted): stop offering buttons.
    public mutating func expire(action id: String) {
        update(action: id) { $0.state = .expired }
    }

    public mutating func expire(approval id: String) {
        update(approval: id) { $0.state = .expired }
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
    /// A scheduled run: the schedule, when, and what it found.
    case report(Report)
    case approval(Approval)
    case action(Action)
    case image(Picture)

    public var id: String {
        switch self {
        case .user(let m): "user-\(m.id)"
        case .assistant(let m): "assistant-\(m.id)"
        case .report(let r): "report-\(r.groupID ?? r.id)"
        case .approval(let a): "approval-\(a.id)"
        case .action(let a): "action-\(a.id)"
        case .image(let p): "image-\(p.id)"
        }
    }

    /// The seq of the event it came from.
    public var seq: Int {
        switch self {
        case .user(let m), .assistant(let m): m.id
        case .report(let r): r.groupID ?? r.id
        case .approval(let a): a.seq
        case .action(let a): a.seq
        case .image(let p): p.id
        }
    }
}

public struct Message: Sendable, Equatable {
    public let id: Int
    public let text: String
    /// The task it started or answered.
    public var task: String? = nil
    /// A scheduled run's answer when it found nothing new: one quiet line.
    public var quiet = false

    /// "Nothing new since 8:00." rather than a report: a single short line.
    static func isOneLine(_ text: String) -> Bool {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return !text.contains("\n") && text.count <= 200
    }
}

/// A scheduled run as the chat shows it: not the user speaking, but the
/// schedule's report. Its prompt is kept as the instructions, out of the way.
public struct Report: Sendable, Equatable {
    /// The seq of the run's prompt.
    public let id: Int
    public let task: String?
    /// The schedule's name.
    public let schedule: String
    /// The prompt the run was given.
    public let instructions: String
    /// When it started (agentd's ISO 8601 timestamp).
    public let ts: String
    /// The report; nil while the run works (or waits on a card).
    public var answer: Message? = nil
    /// Earlier runs of this schedule that found nothing, oldest first, folded
    /// into this one (`visibleItems`).
    public var earlier: [Report] = []
    /// The first folded run's id, which the folded row keeps.
    public var groupID: Int? = nil

    public init(id: Int, task: String?, schedule: String, instructions: String, ts: String) {
        self.id = id
        self.task = task
        self.schedule = schedule
        self.instructions = instructions
        self.ts = ts
    }

    /// It found nothing new, and said so in a line.
    public var quiet: Bool { answer?.quiet == true }

    /// This run on its own, without what's folded into it.
    var alone: Report {
        var report = self
        report.earlier = []
        report.groupID = nil
        return report
    }
}

public struct Approval: Sendable, Equatable {
    public enum State: Sendable, Equatable { case pending, approved, denied, expired }
    public let id: String
    public let seq: Int
    public let command: String
    public let reason: String
    public var state: State = .pending
    /// For a command: what it does in plain words (the agent's), shown
    /// instead of the command, which is a tap away.
    public var summary: String?
    /// Asked for after the agent read outside content (a page, an email,
    /// command output), so its words may have been steered by that content.
    public var tainted = false
    /// Set when it's a browser action (typing, a key, a click) rather than a command.
    public var browser: BrowserAction?
    /// Set when it's a schedule the agent wants to set up.
    public var schedule: ScheduleProposal?
    /// Set when the agent asks for a key or token.
    public var secret: SecretRequest?
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
