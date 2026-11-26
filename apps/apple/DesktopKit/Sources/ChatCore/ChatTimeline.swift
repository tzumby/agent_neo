import AgentAPI
import Foundation

/// The chat as the apps show it, built from agentd's event log: the user's
/// messages, the agent's answers and pictures, scheduled runs as reports, the
/// cards waiting for the user, a task's plan (the steps the agent wrote
/// for itself, ticked off as it goes), and a line for each action an approved
/// plan let go ahead without its card. The same rules as the web app
/// (apps/web/public/assets/js/timeline.js). What the agent does on the way
/// (tool calls, commands, saves) stays out;
/// `busy` only says that it's working. A pure value: apply events in order.
/// Replaying the later part of the log gives the chat of that part, so the
/// apps can load it a page at a time from the newest. One timeline is one
/// conversation's (the store routes events by their `session`).
public struct ChatTimeline: Sendable, Equatable {
    /// The event kinds it reads: the apps ask agentd for these only.
    public static let kinds: Set<String> = [
        "message", "image", "task.started", "task.paused", "task.resumed", "task.stopping", "task.finished",
        "approval.requested", "approval.resolved", "approval.plan", "action_needed", "action.resolved",
        "secret.saved", "plan", "message.queued", "message.unqueued",
    ]

    public private(set) var items: [ChatItem] = []
    /// Messages sent while it was busy, waiting their turn, in order. They
    /// show below the chat until they start (then their message comes as usual).
    public private(set) var queued: [QueuedMessage] = []
    /// Queued messages that left the queue (started, removed or stopped), so
    /// an older list of the queue doesn't bring them back.
    private var unqueued: Set<String> = []
    /// A task is running (between task.started and task.finished, except while
    /// it's paused waiting for an approval).
    public private(set) var busy = false
    /// The user pressed Stop and the task hasn't ended yet.
    public private(set) var stopping = false

    private var approvalIndex: [String: Int] = [:]
    private var actionIndex: [String: Int] = [:]
    /// A scheduled run's report, by task: its answer goes into it.
    private var reportIndex: [String: Int] = [:]
    private var planIndex: [String: Int] = [:]
    /// The running chat task, for "Working · 24 min".
    private var running: Running?

    private struct Running: Sendable, Equatable {
        let task: String?
        /// When it started (nil: agentd's timestamp didn't parse).
        let since: Date?
    }

    public init() {}

    /// The chat from these events, oldest first.
    public init(replaying events: [AgentEvent]) {
        events.forEach { apply($0) }
    }

    /// What the chat shows: messages, pictures, reports, the cards still
    /// waiting for an answer (answered, they leave the chat), and each task's
    /// plan right after its message. Runs of one schedule that found nothing,
    /// one after another, fold into the latest (an hourly check doesn't fill
    /// the chat); the row keeps the first one's id.
    public var visibleItems: [ChatItem] {
        var shown: [ChatItem] = []
        // A plan goes right after its task's message, when that's loaded.
        var asked = Set<String>()
        var plans: [String: Plan] = [:]
        for item in items {
            if case .user(let message) = item, let task = message.task { asked.insert(task) }
            if case .plan(let plan) = item, !plan.steps.isEmpty { plans[plan.task] = plan }
        }
        for item in items {
            switch item {
            case .plan(let plan):
                if !plan.steps.isEmpty, !asked.contains(plan.task) { shown.append(item) }
            case .user(let message) where message.task.flatMap { plans[$0] } != nil:
                shown.append(item)
                shown.append(.plan(plans[message.task!]!))
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

    /// The queued messages to show: the ones the events queued, then any that
    /// agentd listed (GET /sessions, or the answer to a send) and no event
    /// has queued or unqueued yet, e.g. queued before the oldest page loaded.
    public func queue(merging listed: [QueuedMessage]) -> [QueuedMessage] {
        let known = unqueued.union(queued.map(\.id))
        let extra = listed.filter { !known.contains($0.id) }
        guard !extra.isEmpty else { return queued }
        // By when they were queued; the events' order among equals.
        return (queued + extra).enumerated()
            .sorted { ($0.element.created, $0.offset) < ($1.element.created, $1.offset) }
            .map(\.element)
    }

    /// When the running chat task started, while it works.
    public var workingSince: Date? {
        busy ? running?.since : nil
    }

    /// "1 min", "24 min", "1 h 5 min" for whole minutes; nil under one.
    public static func duration(minutes: Double) -> String? {
        guard minutes >= 1, minutes.isFinite else { return nil }
        let m = Int(minutes)
        if m < 60 { return "\(m) min" }
        let rest = m % 60
        return rest == 0 ? "\(m / 60) h" : "\(m / 60) h \(rest) min"
    }

    /// How long the running task has worked, from `workingSince` (nil under a minute).
    public static func working(since: Date?, now: Date = .now) -> String? {
        guard let since else { return nil }
        return duration(minutes: now.timeIntervalSince(since) / 60)
    }

    /// agentd's timestamps: RFC 3339, with or without fractional seconds.
    static func agentDate(_ ts: String) -> Date? {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.date(from: ts) ?? ISO8601DateFormatter().date(from: ts)
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
            running = Running(task: data["task"]?.string, since: Self.agentDate(event.ts))
        case "task.paused":  // parked on a card: the desktop is free meanwhile
            busy = false
            stopping = false
        case "task.resumed":
            busy = true
            // Its start is on an older page: count from here.
            let task = data["task"]?.string
            if running == nil || running?.task != task { running = Running(task: task, since: Self.agentDate(event.ts)) }
        case "task.stopping":
            stopping = busy
        case "task.finished":
            busy = false
            stopping = false
            running = nil
            if let task = data["task"]?.string { update(plan: task) { $0.finished = true } }
        case "plan":
            // Each one is the whole plan: the latest wins.
            guard let task = data["task"]?.string, let list = data["items"]?.array else { break }
            let steps = list.compactMap { PlanStep($0) }
            if planIndex[task] != nil {
                update(plan: task) { $0.steps = steps }
            } else {
                planIndex[task] = items.count
                items.append(.plan(Plan(task: task, seq: event.seq, steps: steps)))
            }
        case "approval.requested":
            let id = data["id"]?.string ?? ""
            var approval = Approval(
                id: id, seq: event.seq, command: data["command"]?.string ?? "", reason: data["reason"]?.string ?? "")
            approval.summary = data["summary"]?.string.flatMap { $0.isEmpty ? nil : $0 }
            approval.tainted = data["tainted"]?.bool ?? false
            approval.editable = data["editable"]?.bool ?? false
            approval.savedKeys = data["secrets"]?.array?.compactMap(\.string).filter { !$0.isEmpty } ?? []
            approval.directInternet = data["direct"]?.bool ?? false
            if data["kind"]?.string != "plan" {
                approval.forTask = data["for_task"]?.string.flatMap { $0.isEmpty ? nil : $0 }
            }
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
            } else if data["kind"]?.string == "access" {
                approval.access = AccessRequest(
                    hosts: data["hosts"]?.array?.compactMap(\.string) ?? [], why: data["why"]?.string ?? "")
            } else if data["kind"]?.string == "stuck" {
                let done = data["plan"]?["done"]?.int ?? 0
                let total = data["plan"]?["total"]?.int ?? 0
                approval.stuck = StuckCheck(
                    reason: data["reason"]?.string ?? "", minutes: data["minutes"]?.int ?? 0,
                    steps: data["steps"]?.int ?? 0, plan: total > 0 ? StuckCheck.Progress(done: done, total: total) : nil)
            } else if data["kind"]?.string == "browser" {
                approval.browser = BrowserAction(data)
            } else if data["kind"]?.string == "app" {
                approval.app = AppAction(data)
            } else if data["kind"]?.string == "plan" {
                approval.plan = PlanApproval(
                    steps: data["steps"]?.array?.compactMap { PlanStep($0) } ?? [], covers: PlanCovers(data["covers"]))
            } else if data["kind"]?.string == "install" {
                // Its summary is the store's description, kept with the store's facts.
                approval.summary = nil
                approval.install = AppInstall(
                    source: data["source"]?.string ?? "", name: data["name"]?.string ?? "",
                    app: data["app"]?.string ?? "", publisher: data["publisher"]?.string ?? "",
                    verified: data["verified"]?.bool ?? false, version: data["version"]?.string ?? "",
                    channel: data["channel"]?.string ?? "", confinement: data["confinement"]?.string ?? "",
                    summary: data["summary"]?.string ?? "",
                    size: data["size"]?.double.flatMap { $0 > 0 ? Int(exactly: $0.rounded()) : nil },
                    why: data["why"]?.string ?? "")
            }
            approvalIndex[id] = items.count
            items.append(.approval(approval))
        case "approval.resolved":
            // Expired: given up (stopped, a new message, or the rules changed).
            let state: Approval.State =
                data["expired"]?.bool == true ? .expired : data["approved"]?.bool == true ? .approved : .denied
            update(approval: data["id"]?.string ?? "") { $0.state = state }
        case "approval.plan":
            // An action that went ahead without its card: the plan the user
            // approved covers it. What that card would have shown, in a line.
            items.append(.covered(CoveredAction(seq: event.seq, data)))
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
        case "message.queued":
            guard let id = data["id"]?.string, !unqueued.contains(id), !queued.contains(where: { $0.id == id })
            else { break }
            let created = data["created"]?.double.map(Date.init(timeIntervalSince1970:)) ?? Self.agentDate(event.ts)
            queued.append(
                QueuedMessage(
                    id: id, text: data["text"]?.string ?? "", by: data["by"]?.string, created: created ?? .distantPast))
        case "message.unqueued":
            // Started (its message follows), removed, or dropped by Stop.
            guard let id = data["id"]?.string else { break }
            unqueued.insert(id)
            queued.removeAll { $0.id == id }
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

    private mutating func update(plan task: String, _ change: (inout Plan) -> Void) {
        guard let index = planIndex[task], case .plan(var plan) = items[index] else { return }
        change(&plan)
        items[index] = .plan(plan)
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
    /// A task's plan: the agent's own steps, ticked off as it goes.
    case plan(Plan)
    /// An action that went ahead without its card, under an approved plan.
    case covered(CoveredAction)

    public var id: String {
        switch self {
        case .user(let m): "user-\(m.id)"
        case .assistant(let m): "assistant-\(m.id)"
        case .report(let r): "report-\(r.groupID ?? r.id)"
        case .approval(let a): "approval-\(a.id)"
        case .action(let a): "action-\(a.id)"
        case .image(let p): "image-\(p.id)"
        case .plan(let p): "plan-\(p.task)"
        case .covered(let c): "covered-\(c.seq)"
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
        case .plan(let p): p.seq
        case .covered(let c): c.seq
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
    /// The agent's description, shown alongside the command.
    public var summary: String?
    /// Saved credential names available to the command.
    public var savedKeys: [String] = []
    /// The command can connect outside the egress proxy.
    public var directInternet = false
    /// Asked for after the agent read outside content (a page, an email,
    /// command output), so its words may have been steered by that content.
    public var tainted = false
    /// The user can change its text before approving: a command, what to type,
    /// a schedule's instructions. Only a card that says so: an older agentd
    /// would ignore the edit and run the agent's words.
    public var editable = false
    /// Set when it's a browser action (typing, a key, a click) rather than a command.
    public var browser: BrowserAction?
    /// Set when it's a click (or typing, or a key) in one of the desktop's own apps.
    public var app: AppAction?
    /// Set when the agent asks to install an app on the desktop, from a store.
    public var install: AppInstall?
    /// Set when it's a schedule the agent wants to set up.
    public var schedule: ScheduleProposal?
    /// Set when the agent asks for a key or token.
    public var secret: SecretRequest?
    /// Set when the agent asks to reach sites the desktop's proxy refuses.
    public var access: AccessRequest?
    /// Set when a check on the task found it going in circles: carry on or stop.
    public var stuck: StuckCheck?
    /// Set when it's the task's plan, to approve as a whole.
    public var plan: PlanApproval?
    /// "Allow for this task": what approving with it would also let through
    /// until the task ends, in words; nil on a card no grant covers.
    public var forTask: String?
}

/// "Looks stuck": a check on the running task (agentd's supervisor) found it
/// going in circles. Approving lets it carry on, with a hint; denying stops it
/// with what's done and what's left. Never answered for the user.
public struct StuckCheck: Sendable, Equatable {
    /// Why it looks stuck, in a sentence.
    public let reason: String
    /// How long it has worked, in minutes.
    public let minutes: Int
    /// How many steps it has taken.
    public let steps: Int
    /// Its plan's progress, when it has one.
    public let plan: Progress?

    public struct Progress: Sendable, Equatable {
        public let done: Int
        public let total: Int
    }

    public init(reason: String, minutes: Int, steps: Int, plan: Progress?) {
        self.reason = reason
        self.minutes = minutes
        self.steps = steps
        self.plan = plan
    }
}

/// The steps the agent wrote for itself (its `plan` tool), as the latest
/// `plan` event has them.
public struct Plan: Sendable, Equatable {
    public let task: String
    /// The seq of its first event.
    public let seq: Int
    public var steps: [PlanStep]
    /// The task ended: the plan folds away.
    public var finished = false

    public init(task: String, seq: Int, steps: [PlanStep], finished: Bool = false) {
        self.task = task
        self.seq = seq
        self.steps = steps
        self.finished = finished
    }

    public var done: Int { steps.filter { $0.status == .done }.count }
}

public struct PlanStep: Sendable, Equatable {
    public enum Status: String, Sendable { case pending, inProgress = "in_progress", done }
    public var step: String
    public var status: Status
    /// What it needs the user's OK for, when it says: a site (clicks, typing
    /// and keys there), an app (input in it), commands that would ask, the
    /// saved keys they use, an install. A plan card approves them for the
    /// rest of the task. `commands`: exact ones, from agentd before 0.19.1.
    public var site: String?
    public var app: String?
    public var shell = false
    public var keys: [String] = []
    public var commands: [String] = []
    public var install: String?

    public init(
        step: String, status: Status, site: String? = nil, app: String? = nil, shell: Bool = false,
        keys: [String] = [], commands: [String] = [], install: String? = nil
    ) {
        self.step = step
        self.status = status
        self.site = site
        self.app = app
        self.shell = shell
        self.keys = keys
        self.commands = commands
        self.install = install
    }

    /// A step as agentd sends it, with what's odd left out; nil without words.
    init?(_ json: JSONValue) {
        guard let step = json["step"]?.string, !step.trimmingCharacters(in: .whitespaces).isEmpty else { return nil }
        let named = { (key: String) in json[key]?.string.flatMap { $0.isEmpty ? nil : $0 } }
        var seen = Set<String>()
        self.init(
            step: step, status: Status(rawValue: json["status"]?.string ?? "") ?? .pending, site: named("site"),
            app: named("app"), shell: json["shell"]?.bool == true,
            keys: json["keys"]?.array?.compactMap(\.string).filter { !$0.isEmpty && seen.insert($0).inserted } ?? [],
            commands: json["commands"]?.array?.compactMap(\.string).filter { !$0.isEmpty } ?? [],
            install: named("install"))
    }

    /// Its needs in order, one per chip: one per key and per command.
    public var needs: [PlanNeed] {
        [site.map { PlanNeed(kind: .site, value: $0) }, app.map { PlanNeed(kind: .app, value: $0) }].compactMap { $0 }
            + (shell ? [PlanNeed(kind: .shell, value: "Commands that would ask")] : [])
            + keys.map { PlanNeed(kind: .key, value: $0) }
            + commands.map { PlanNeed(kind: .command, value: $0) }
            + (install.map { [PlanNeed(kind: .install, value: $0)] } ?? [])
    }

    /// The step without its `index`th need (a chip's ×).
    public func removing(need index: Int) -> PlanStep {
        var step = PlanStep(step: self.step, status: status)
        for (i, need) in needs.enumerated() where i != index {
            switch need.kind {
            case .site: step.site = need.value
            case .app: step.app = need.value
            case .shell: step.shell = true
            case .key: step.keys.append(need.value)
            case .command: step.commands.append(need.value)
            case .install: step.install = need.value
            }
        }
        return step
    }

    /// As agentd takes it back, in a plan card's answer.
    var edit: PlanEdit {
        PlanEdit(
            step: step, status: status.rawValue, site: site, app: app, shell: shell ? true : nil,
            keys: keys.isEmpty ? nil : keys, commands: commands.isEmpty ? nil : commands, install: install)
    }
}

/// One thing a plan step needs the user's OK for: a chip on the plan card.
public struct PlanNeed: Sendable, Equatable {
    public enum Kind: Sendable { case site, app, shell, key, command, install }
    public let kind: Kind
    public let value: String

    public init(kind: Kind, value: String) {
        self.kind = kind
        self.value = value
    }
}

/// A plan card: the task's plan, approved as a whole. What its steps need then
/// goes ahead without cards of its own for the rest of the task; anything
/// else still asks, and direct internet, proxy sites and schedules always do.
public struct PlanApproval: Sendable, Equatable {
    /// The whole plan, as the agent wrote it.
    public let steps: [PlanStep]
    /// What approving adds for the task (a later card's: only what's new).
    public let covers: PlanCovers
}

/// What a plan covers, each list without repeats.
public struct PlanCovers: Sendable, Equatable {
    public var sites: [String] = []
    public var apps: [String] = []
    /// Commands that would ask, any of them (not direct ones).
    public var shell = false
    /// Commands using these saved keys.
    public var keys: [String] = []
    public var commands: [String] = []
    public var installs: [String] = []

    public init(
        sites: [String] = [], apps: [String] = [], shell: Bool = false, keys: [String] = [], commands: [String] = [],
        installs: [String] = []
    ) {
        self.sites = sites
        self.apps = apps
        self.shell = shell
        self.keys = keys
        self.commands = commands
        self.installs = installs
    }

    /// agentd's `covers`, any of whose lists may be missing.
    init(_ json: JSONValue?) {
        func list(_ key: String) -> [String] {
            var seen = Set<String>()
            return (json?[key]?.array ?? []).compactMap(\.string).filter { !$0.isEmpty && seen.insert($0).inserted }
        }
        self.init(
            sites: list("sites"), apps: list("apps"), shell: json?["shell"]?.bool == true, keys: list("keys"),
            commands: list("commands"), installs: list("installs"))
    }

    /// Only what some step still needs: the user may have taken needs off.
    public func needed(by steps: [PlanStep]) -> PlanCovers {
        let needs = steps.flatMap(\.needs)
        func left(_ kind: PlanNeed.Kind, _ list: [String]) -> [String] {
            let values = Set(needs.filter { $0.kind == kind }.map(\.value))
            return list.filter(values.contains)
        }
        return PlanCovers(
            sites: left(.site, sites), apps: left(.app, apps), shell: shell && needs.contains { $0.kind == .shell },
            keys: left(.key, keys), commands: left(.command, commands), installs: left(.install, installs))
    }

    /// The plan card's summary, a phrase per kind of need. None: approving
    /// only starts the plan.
    public var phrases: [Phrase] {
        [
            Phrase(lead: "clicking, typing and pressing keys on ", items: sites),
            Phrase(lead: "input in ", items: apps),
            Phrase(lead: "running commands that would ask (deletes, pushes, uploads)", items: [], on: shell),
            Phrase(lead: "commands using your key ", items: keys, code: true),
            Phrase(lead: "running ", items: commands, code: true),
            Phrase(lead: "installing ", items: installs),
        ].filter { !$0.items.isEmpty || $0.on }
    }

    /// "running `a` and `b`": `lead`, then its items in a list.
    public struct Phrase: Sendable, Equatable {
        public let lead: String
        public let items: [String]
        /// The items are commands or keys, shown as code.
        public var code = false
        /// Said even with no items (commands that would ask).
        public var on = false

        /// The phrase in pieces: the words, and each item (as code, or not).
        public var pieces: [Piece] {
            [Piece(text: lead)]
                + items.enumerated().flatMap { index, item in
                    let gap = index == 0 ? nil : index == items.count - 1 ? " and " : ", "
                    return [gap.map { Piece(text: $0) }, Piece(text: item, code: code)].compactMap { $0 }
                }
        }

        public struct Piece: Sendable, Equatable {
            public let text: String
            public var code = false
        }
    }
}

/// The user's version of a plan card's steps while they edit it: reworded,
/// deleted, added, moved, needs taken off. None can be added (agentd refuses
/// a need the card didn't have), so a new step has none.
public struct PlanDraft: Sendable, Equatable {
    public struct Step: Sendable, Equatable, Identifiable {
        public let id: Int
        public var plan: PlanStep
    }

    /// agentd's limits.
    public static let maxSteps = 20
    public static let maxLength = 120

    public var steps: [Step]
    private var nextID: Int

    public init(_ steps: [PlanStep]) {
        self.steps = steps.enumerated().map { Step(id: $0.offset, plan: $0.element) }
        nextID = steps.count
    }

    public var canAdd: Bool { steps.count < Self.maxSteps }

    /// An empty step at the end. Returns its id, to focus it.
    @discardableResult
    public mutating func add() -> Int {
        defer { nextID += 1 }
        steps.append(Step(id: nextID, plan: PlanStep(step: "", status: .pending)))
        return nextID
    }

    public mutating func delete(_ id: Int) {
        steps.removeAll { $0.id == id }
    }

    /// Up (-1) or down (1) a place.
    public mutating func move(_ id: Int, by offset: Int) {
        guard let index = steps.firstIndex(where: { $0.id == id }), steps.indices.contains(index + offset) else {
            return
        }
        steps.swapAt(index, index + offset)
    }

    /// A step's words as typed: one line, at most `maxLength` characters.
    public mutating func set(_ text: String, of id: Int) {
        guard let index = steps.firstIndex(where: { $0.id == id }) else { return }
        steps[index].plan.step = String(text.replacingOccurrences(of: "\n", with: " ").prefix(Self.maxLength))
    }

    public mutating func removeNeed(_ need: Int, from id: Int) {
        guard let index = steps.firstIndex(where: { $0.id == id }) else { return }
        steps[index].plan = steps[index].plan.removing(need: need)
    }

    /// Why it can't be approved as it is (agentd would refuse it), or nil.
    public var problem: String? {
        if steps.isEmpty { return "A plan needs a step. Add one, or deny it." }
        if steps.count > Self.maxSteps { return "A plan has at most \(Self.maxSteps) steps." }
        if steps.contains(where: { $0.plan.step.trimmingCharacters(in: .whitespaces).isEmpty }) {
            return "Write the empty step, or delete it."
        }
        return nil
    }

    /// What approving lets the task do without asking: the card's `covers`,
    /// less what the edits took off every step.
    public func covered(by covers: PlanCovers) -> PlanCovers {
        covers.needed(by: steps.map(\.plan))
    }

    /// The steps to send with the approval, their words trimmed: nil when
    /// they're the agent's own (only a change is sent).
    public func edits(from original: [PlanStep]) -> [PlanEdit]? {
        let trim = { (step: PlanStep) in
            var step = step
            step.step = step.step.trimmingCharacters(in: .whitespaces)
            return step
        }
        let mine = steps.map { trim($0.plan) }
        return mine == original.map(trim) ? nil : mine.map(\.edit)
    }
}

/// The agent's commands hit sites the desktop's proxy doesn't let through:
/// allowed, they're open from then on (scheduled checks too), until removed
/// in the web app's Settings.
public struct AccessRequest: Sendable, Equatable {
    /// Exact host names, e.g. status.example.com.
    public let hosts: [String]
    /// What the agent needs them for.
    public let why: String

    public init(hosts: [String], why: String) {
        self.hosts = hosts
        self.why = why
    }
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
    /// An older server bundled typing and sending.
    public let sends: Bool
    /// The page's host, e.g. app.slack.com.
    public let site: String
    /// What it is ("button"), for a click.
    public var role = ""
    /// The page's title and address.
    public var page = ""
    public var url = ""
    /// The link or form target, when applicable.
    public var destination = ""
    /// The text around it on the page: the dialog, row or form it's in.
    public var around = ""
    /// The page, with what it would click outlined.
    public var imagePath: String?
}

extension BrowserAction {
    /// What a browser card (or an action a plan covered) says.
    init(_ data: JSONValue) {
        self.init(
            action: data["action"]?.string ?? "", target: data["target"]?.string ?? "", text: data["text"]?.string,
            key: data["key"]?.string, submit: data["submit"]?.bool ?? false, sends: data["sends"]?.bool ?? false,
            site: data["site"]?.string ?? "")
        role = data["role"]?.string ?? ""
        page = data["page"]?.string ?? ""
        url = data["url"]?.string ?? ""
        destination = data["destination"]?.string ?? ""
        around = data["around"]?.string ?? ""
        imagePath = data["image"]?.string
    }
}

/// A change the agent wants to make in one of the desktop's own apps, which
/// it drives through their accessibility tree, or (action "gui", with a model
/// that sees the screen) by pixels of a screenshot. What it says about where
/// comes from the app, not the agent.
public struct AppAction: Sendable, Equatable {
    /// "click", "type", "key", "set_value", "select", "scroll", "activate" or
    /// "close" for one input; "act" for a batch; "gui" for pixel input.
    public let action: String
    /// The app's name, e.g. Mousepad.
    public let app: String
    /// The window's title.
    public let window: String
    /// The element's accessible name (empty when it has none).
    public let target: String
    /// What it is ("menuitem", "push button").
    public let role: String
    /// What it will type.
    public let text: String?
    public let key: String?
    /// Where it sits in the app: its menus or panels, and its description.
    public let around: String
    /// The desktop when the agent asked.
    public let imagePath: String?
    /// Every step, in order. Empty on an older card: one click, type or key.
    public var steps: [AppStep] = []
    /// For "gui": the pixel size of the picture, which the steps' points are in.
    public var frame: Frame?

    public struct Frame: Sendable, Equatable {
        public let width: Double
        public let height: Double
    }

    /// The card lists its steps: a batch, or any pixel input.
    public var listsSteps: Bool { steps.count > 1 || action == "act" || action == "gui" }

    /// Where each pixel step acts on the picture, in fractions of its size, by
    /// the step's number. None but on a "gui" card with its picture and size.
    public var marks: [Mark] {
        guard action == "gui", imagePath != nil, let frame else { return [] }
        return steps.enumerated().compactMap { index, step in
            step.points.isEmpty
                ? nil
                : Mark(
                    step: index + 1,
                    points: step.points.map {
                        AppStep.Point(
                            x: min(max($0.x / frame.width, 0), 1), y: min(max($0.y / frame.height, 0), 1))
                    })
        }
    }

    /// A step's place on the picture: one point, or a drag's path.
    public struct Mark: Sendable, Equatable {
        public let step: Int
        public let points: [AppStep.Point]
    }
}

extension AppAction {
    /// What an app card (or an action a plan covered) says. A batch has all
    /// its steps; a lone input's are on the card too.
    init(_ data: JSONValue) {
        self.init(
            action: data["action"]?.string ?? "", app: data["app"]?.string ?? "",
            window: data["window"]?.string ?? "", target: data["target"]?.string ?? "",
            role: data["role"]?.string ?? "", text: data["text"]?.string, key: data["key"]?.string,
            around: data["around"]?.string ?? "", imagePath: data["image"]?.string)
        steps = data["steps"]?.array?.compactMap(AppStep.init) ?? []
        if let width = data["frame"]?["width"]?.double, let height = data["frame"]?["height"]?.double,
            width > 0, height > 0
        {
            frame = Frame(width: width, height: height)
        }
    }
}

/// An action that went ahead without its card: the plan the user approved
/// covers it (`approval.plan`). What that card would have shown, as a quiet
/// line in the chat.
public struct CoveredAction: Sendable, Equatable {
    public let seq: Int
    public let task: String?
    /// The plan card that covers it, and who approved that.
    public let approval: String
    public let by: String
    /// "browser", "app", "shell" or "install".
    public let kind: String
    public let summary: String?
    public var browser: BrowserAction?
    public var app: AppAction?
    /// A shell action's command.
    public var command = ""
    /// An install's snap name (what got installed) and its title.
    public var installName = ""
    public var installTitle = ""

    init(seq: Int, _ data: JSONValue) {
        self.seq = seq
        task = data["task"]?.string
        approval = data["approval"]?.string ?? ""
        by = data["by"]?.string ?? ""
        let card = data["card"] ?? .null
        kind = card["kind"]?.string ?? ""
        summary = card["summary"]?.string.flatMap { $0.isEmpty ? nil : $0 }
        command = card["command"]?.string ?? ""
        switch kind {
        case "browser": browser = BrowserAction(card)
        case "app": app = AppAction(card)
        case "install":
            installName = card["name"]?.string ?? ""
            installTitle = card["app"]?.string ?? ""
        default: break
        }
    }

    /// The line the chat shows: where it acted (the site or app; empty for a
    /// command or an install), what it did (the summary, else the target or
    /// the command), and whether there's more to unfold.
    public struct Line: Sendable, Equatable {
        public let place: String
        public let what: String
        /// `what` is a command, shown as code.
        public var code = false
        public var more = false
    }

    public var line: Line {
        if let browser {
            let said =
                switch browser.action {
                case "type": browser.target.isEmpty ? "Write" : "Write in “\(browser.target)”"
                case "key": "Press \(browser.key ?? "a key")"
                default: "Click \(AppStep.clickTarget(role: browser.role, name: browser.target))"
                }
            let what = summary ?? said
            let more = browser.text != nil || browser.key != nil || (summary != nil && !browser.target.isEmpty)
            return Line(place: browser.site, what: what, more: more)
        }
        if let app {
            let count = app.steps.count
            let what =
                summary
                ?? (!app.target.isEmpty
                    ? AppStep.clickTarget(role: app.role, name: app.target)
                    : count > 0 ? "\(count) \(count == 1 ? "step" : "steps")" : app.window.isEmpty ? "Input" : app.window)
            return Line(place: app.app, what: what, more: count > 0 || !app.window.isEmpty || app.text != nil)
        }
        if kind == "install" {
            // Its summary, if any, would be the store's: the name says it.
            let name = !installTitle.isEmpty ? installTitle : !installName.isEmpty ? installName : "an app"
            return Line(place: "", what: "Install \(name)", more: !installTitle.isEmpty && !installName.isEmpty)
        }
        if let summary { return Line(place: "", what: summary, more: !command.isEmpty && command != summary) }
        let first = command.components(separatedBy: "\n").first ?? ""
        // A kind from a newer agentd says nothing more.
        return first.isEmpty
            ? Line(place: "", what: "An action")
            : Line(place: "", what: first, code: true, more: first != command)
    }
}

/// One step of an app card, with what's missing or odd left empty.
public struct AppStep: Sendable, Equatable {
    public var action: String
    /// The element: its accessible name and what it is ("menuitem").
    public var target = ""
    public var role = ""
    /// Where it sits in the app.
    public var around = ""
    /// What it writes, sets or selects, in full.
    public var text: String?
    /// The key, or keys, pressed ("CTRL+S"); for a pixel click, the keys held.
    public var key: String?
    /// The window it acts in (or switches to, or closes).
    public var window = ""
    /// A click with the mouse rather than through the app.
    public var pointer = false
    /// A selection from the element to another.
    public var range = false
    /// A scroll's direction and pages.
    public var direction = ""
    public var pages: Int?
    /// What a wait waits for ("a window “Save As”"), and whether for it to go.
    public var waitFor = ""
    public var gone = false
    /// A pixel click's mouse button ("right").
    public var button = ""
    /// A pixel step's point, or a drag's path, in the picture's pixels.
    public var points: [Point] = []

    public struct Point: Sendable, Equatable {
        public let x: Double
        public let y: Double
    }

    public init(action: String) { self.action = action }

    init?(_ json: JSONValue) {
        guard case .object = json else { return nil }
        action = json["action"]?.string ?? ""
        target = json["target"]?.string ?? ""
        role = json["role"]?.string ?? ""
        around = json["around"]?.string ?? ""
        text = json["text"]?.string
        let keys = json["keys"]?.array?.compactMap(\.string).filter { !$0.isEmpty }.joined(separator: "+") ?? ""
        key = json["key"]?.string.flatMap { $0.isEmpty ? nil : $0 } ?? (keys.isEmpty ? nil : keys)
        window = json["window"]?.string ?? ""
        pointer = json["pointer"]?.bool ?? false
        range = json["range"]?.bool ?? false
        direction = json["direction"]?.string ?? ""
        pages = json["pages"]?.double.flatMap { $0 > 0 ? Int(exactly: $0.rounded()) : nil }
        waitFor = json["for"]?.string ?? ""
        gone = json["gone"]?.bool ?? false
        button = json["button"]?.string ?? ""
        let point = { (p: JSONValue) -> Point? in
            guard let x = p["x"]?.double, let y = p["y"]?.double, x.isFinite, y.isFinite else { return nil }
            return Point(x: x, y: y)
        }
        points = json["path"]?.array.map { $0.compactMap(point) } ?? point(json).map { [$0] } ?? []
    }

    /// A step in plain words: `text`, then a keycap for `key`; the full
    /// `quote` it writes; muted, where it is in the app (`note`) and a window
    /// other than the card's (`window`). A wait sends nothing: `quiet`.
    public struct Words: Sendable, Equatable {
        public var text = ""
        public var key: String?
        public var quote: String?
        public var note: String?
        public var window: String?
        public var quiet = false
    }

    /// `pixels`: a step of a "gui" card, at a point of the card's picture.
    public func words(pixels: Bool = false, window cardWindow: String = "") -> Words {
        var words = Words()
        if !window.isEmpty, window != cardWindow, action != "activate", action != "close" {
            words.window = "In “\(window)”"
        }
        let what = Self.element(role: role, name: target)
        if pixels {
            let spot = { (p: Point) in "\(Self.pixel(p.x)), \(Self.pixel(p.y))" }
            let at = { (p: Point?) in p.map { " at \(spot($0))" } ?? "" }
            switch action {
            case "click":
                words.text =
                    "Click\(at(points.first))\(button.isEmpty || button == "left" ? "" : " (\(button) button)")"
                    + (key.map { ", holding \($0)" } ?? "")
            case "double_click": words.text = "Double-click\(at(points.first))"
            case "move": words.text = "Point\(at(points.first))"
            case "drag":
                if let first = points.first, let last = points.last, points.count > 1 {
                    words.text = "Drag from \(spot(first)) to \(spot(last))"
                } else {
                    words.text = "Drag"
                }
            case "scroll": words.text = "Scroll\(at(points.first))"
            case "type":
                words.text = "Write:"
                words.quote = text ?? ""
            case "keypress":
                words.text = "Press"
                words.key = key ?? "?"
            case "wait":
                words.text = "Wait a second"
                words.quiet = true
            default: words.text = Self.capitalised(action.isEmpty ? "a step" : action)
            }
            return words
        }
        words.note = around.isEmpty ? nil : around
        switch action {
        case "click":
            words.text = "Click \(Self.clickTarget(role: role, name: target))\(pointer ? " with the mouse" : "")"
        case "type":
            words.text = what.isEmpty ? "Write:" : "Write in \(what):"
            words.quote = text ?? ""
        case "key":
            words.text = "Press"
            words.key = key ?? "?"
        case "set_value":
            words.text = what.isEmpty ? "Set it to:" : "Set \(what) to:"
            words.quote = text ?? ""
        case "select":
            words.text =
                if range {
                    "Select from \(what.isEmpty ? "here" : what)…"
                } else if let text, !text.isEmpty {
                    "Select “\(text)”\(what.isEmpty ? "" : " in \(what)")"
                } else {
                    "Select \(Self.clickTarget(role: role, name: target))"
                }
        case "scroll":
            let count = pages.map { $0 == 1 ? "1 page" : "\($0) pages" } ?? ""
            words.text = ["Scroll", direction, count, what.isEmpty ? "" : "in \(what)"]
                .filter { !$0.isEmpty }
                .joined(separator: " ")
        case "activate": words.text = window.isEmpty ? "Switch windows" : "Switch to the window “\(window)”"
        case "close": words.text = window.isEmpty ? "Close the window" : "Close the window “\(window)”"
        case "wait":
            words.text = "Wait\(waitFor.isEmpty ? "" : " for \(waitFor)")\(gone ? " to go away" : "")"
            words.quiet = true
        default:
            words.text = [Self.capitalised(action.isEmpty ? "a step" : action), what]
                .filter { !$0.isEmpty }
                .joined(separator: " ")
        }
        return words
    }

    /// Button “Confirm”; “without a name” when it has none.
    public static func clickTarget(role: String, name: String) -> String {
        let name = name.isEmpty ? "without a name" : "“\(name)”"
        return role.isEmpty ? name : "\(capitalised(role)) \(name)"
    }

    /// Textbox “Notes”, or just Textbox when it has no name ("" when it has neither).
    static func element(role: String, name: String) -> String {
        [capitalised(role), name.isEmpty ? "" : "“\(name)”"].filter { !$0.isEmpty }.joined(separator: " ")
    }

    static func capitalised(_ text: String) -> String { text.prefix(1).uppercased() + text.dropFirst() }

    /// A pixel, rounded (0 for one no Int holds).
    static func pixel(_ value: Double) -> Int { Int(exactly: value.rounded()) ?? 0 }
}

/// An app the agent wants to install on the desktop, as root, from a store
/// (the Snap Store). All but `why` is the store's, which the agent's words
/// can't change.
public struct AppInstall: Sendable, Equatable {
    /// Where it comes from, e.g. Snap Store.
    public let source: String
    /// The snap's name, which is what gets installed, e.g. slack.
    public let name: String
    /// Its title, e.g. Slack.
    public let app: String
    /// The publisher's display name.
    public let publisher: String
    /// The store verified the publisher.
    public let verified: Bool
    public let version: String
    /// The store channel, e.g. stable.
    public let channel: String
    /// "strict": it runs in the Snap sandbox.
    public let confinement: String
    /// The store's one-line description (empty when it has none).
    public let summary: String
    /// The download size in bytes, when the store gave one.
    public let size: Int?
    /// The agent's reason (empty when it gave none).
    public let why: String

    /// "Version 4.53.292 · stable · 233 MB", leaving out what's missing.
    public var details: String {
        [version.isEmpty ? "" : "Version \(version)", channel, size.map(Self.megabytes) ?? ""]
            .filter { !$0.isEmpty }
            .joined(separator: " · ")
    }

    /// A download size: "233 MB", with one decimal under 10 MB ("4.2 MB").
    public static func megabytes(_ bytes: Int) -> String {
        let mb = Double(bytes) / 1_000_000
        if mb >= 10 { return "\(Int(mb.rounded())) MB" }
        let tenths = max(1, Int((mb * 10).rounded()))
        return tenths % 10 == 0 ? "\(tenths / 10) MB" : "\(tenths / 10).\(tenths % 10) MB"
    }
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
