import Foundation

/// GET /api/v1/status from agentd (through tailnet-proxy).
/// Something the agent remembers across conversations.
public struct AgentMemory: Decodable, Sendable, Equatable, Identifiable {
    public let id: Int
    public let text: String
    /// False: saved after the agent read outside content (a page, an email,
    /// command output); the agent treats it as a hint until the user confirms it.
    public let trusted: Bool
    public let created: String
    // Memory v2 (nil from a desktop that doesn't have it on).
    /// rule, preference, fact or work_log.
    public let kind: String?
    /// active, superseded or expired.
    public let status: String?
    /// user_stated, agent_inferred or imported.
    public let trust: String?
    /// For edits: the version the edit is based on.
    public let version: Int?
    /// Unix seconds; after it the memory expires.
    public let validUntil: Int?
    /// The task that saved it (to open that conversation).
    public let sourceRun: String?
    /// Who wrote it first: agent, user or consolidator.
    public let origin: String?

    enum CodingKeys: String, CodingKey {
        case id, text, trusted, created, kind, status, trust, version, origin
        case validUntil = "valid_until"
        case sourceRun = "source_run"
    }

    public init(
        id: Int, text: String, trusted: Bool, created: String, kind: String? = nil, status: String? = nil,
        trust: String? = nil, version: Int? = nil, validUntil: Int? = nil, sourceRun: String? = nil,
        origin: String? = nil
    ) {
        self.id = id
        self.text = text
        self.trusted = trusted
        self.created = created
        self.kind = kind
        self.status = status
        self.trust = trust
        self.version = version
        self.validUntil = validUntil
        self.sourceRun = sourceRun
        self.origin = origin
    }

    /// Expired or replaced: it can be restored.
    public var isGone: Bool { status == "expired" || status == "superseded" }
    /// A hint until the user confirms it: saved by the agent (after it read
    /// outside content) or imported. agentd's confirm accepts both.
    public var needsConfirmation: Bool {
        trust.map { $0 == "agent_inferred" || $0 == "imported" } ?? !trusted
    }
    public var endDate: Date? { validUntil.map { Date(timeIntervalSince1970: TimeInterval($0)) } }
}

/// Memory consolidation's settings and record (Settings → Memory).
public struct ConsolidationStats: Decodable, Sendable, Equatable {
    public let mode: String
    public let model: String
    public let dailyCapUsd: Double
    public let monthCost: Double
    public let lastRun: LastRun?
    public let waitingTasks: Int

    public struct LastRun: Decodable, Sendable, Equatable {
        public let at: String
        public let status: String
        public let cost: Double?
        public let counts: Counts?
    }

    public struct Counts: Decodable, Sendable, Equatable {
        public let saved: Int
        public let merged: Int
        public let expired: Int
    }

    enum CodingKeys: String, CodingKey {
        case mode, model
        case dailyCapUsd = "daily_cap_usd"
        case monthCost = "month_cost"
        case lastRun = "last_run"
        case waitingTasks = "waiting_tasks"
    }
}

/// What an import did.
public struct MemoryImport: Decodable, Sendable, Equatable {
    public let imported: Int
    public let rejected: [Rejected]

    public struct Rejected: Decodable, Sendable, Equatable {
        public let text: String
        public let reason: String
    }
}

/// Any JSON object, when only the status code matters.
private struct Ignored: Decodable {}

/// A task the agent runs on its own: on a repeat, or once.
public struct AgentSchedule: Decodable, Sendable, Equatable, Identifiable {
    public let id: Int
    public let name: String
    public let prompt: String
    /// Five-field cron pattern; nil for a one-time run.
    public let cron: String?
    /// Where its times are read (the user's time zone when it was set up).
    public let timezone: String
    /// "Weekdays at 08:00", or "Once, Mon Nov 2 at 09:00".
    public let description: String
    public let nextRun: Date
    public let lastRun: Date?
    public let paused: Bool
    /// `agent_tainted`: approved through a card after the agent read outside
    /// content (nil from an older desktop).
    public let createdFrom: String?

    public var repeats: Bool { cron != nil }

    enum CodingKeys: String, CodingKey {
        case id, name, prompt, cron, timezone, description, paused
        case nextRun = "next_run"
        case lastRun = "last_run"
        case createdFrom = "created_from"
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(Int.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        prompt = try c.decode(String.self, forKey: .prompt)
        cron = try c.decodeIfPresent(String.self, forKey: .cron)
        timezone = try c.decode(String.self, forKey: .timezone)
        description = try c.decode(String.self, forKey: .description)
        nextRun = Date(timeIntervalSince1970: try c.decode(Double.self, forKey: .nextRun))
        lastRun = try c.decodeIfPresent(Double.self, forKey: .lastRun).map(Date.init(timeIntervalSince1970:))
        paused = try c.decode(Bool.self, forKey: .paused)
        createdFrom = try c.decodeIfPresent(String.self, forKey: .createdFrom)
    }
}

/// A key or token saved on the desktop (a GitHub token, an API key). The app
/// can save, replace and delete one, but never read its value back.
public struct AgentSecret: Decodable, Sendable, Equatable, Identifiable {
    /// The environment variable commands get it as, e.g. GITHUB_TOKEN.
    public let name: String
    /// What it is: "GitHub token".
    public let label: String
    public let created: String
    public let lastUsed: String?

    public var id: String { name }

    enum CodingKeys: String, CodingKey {
        case name, label, created
        case lastUsed = "last_used"
    }

    public init(name: String, label: String, created: String = "", lastUsed: String? = nil) {
        self.name = name
        self.label = label
        self.created = created
        self.lastUsed = lastUsed
    }
}

/// What saving a key did: whether the task that asked for it carried on.
public struct SavedSecret: Decodable, Sendable, Equatable {
    public let secret: AgentSecret
    /// The task waiting for it resumed.
    public let resumed: Bool
    /// That task couldn't resume yet: another one is running.
    public let busy: Bool
}

/// A model provider (Anthropic, Together), its models, and where its key
/// comes from (agentd 0.20.0). Never the key itself.
public struct ModelProvider: Decodable, Sendable, Equatable, Identifiable {
    public let id: String
    public let name: String
    public let models: [String]
    /// Memory tidy-up's smaller model.
    public let helper: String
    /// "app" (pasted in an app), "deployment" (the desktop's setup), or nil:
    /// no key yet, so picking one of its models asks for it.
    public let key: String?

    public var hasKey: Bool { key != nil }
    public var ref: ProviderRef { ProviderRef(id: id, name: name) }

    public init(id: String, name: String, models: [String], helper: String = "", key: String? = nil) {
        self.id = id
        self.name = name
        self.models = models
        self.helper = helper
        self.key = key
    }
}

/// A provider by id and name: the one whose key the desktop needs.
public struct ProviderRef: Decodable, Sendable, Equatable, Identifiable, Hashable {
    public let id: String
    public let name: String

    public init(id: String, name: String) {
        self.id = id
        self.name = name
    }

    /// What its key is called: "Anthropic API key", "Together AI API key".
    public var keyLabel: String { id == "together" ? "Together AI API key" : "\(name) API key" }
}

/// What saving a model key did. `checked` false: the provider couldn't be
/// asked whether it takes the key (it was saved anyway).
public struct SavedModelKey: Decodable, Sendable, Equatable {
    public let checked: Bool
    public let providers: [ModelProvider]
}

public struct AgentStatus: Decodable, Sendable, Equatable {
    public let desktop: String
    public let busy: Bool
    public let settings: AgentSettings
    public let lastSeq: Int
    public let knownModels: [String]
    public let shellTool: Bool
    public let version: String
    /// One word for what the desktop needs (needs_you/approval, working,
    /// problem, review, scheduled, idle…), and the ids behind it.
    public let stage: String?
    public let stageDetail: JSONValue?
    /// Memory v2 is on.
    public let memoryV2: Bool?
    /// How many conversations work, wait and have queued messages (agentd 0.16.0).
    public let sessions: SessionCounts?
    /// The monthly budget and this month's spending (agentd 0.16.0).
    public let budget: Budget?
    /// Every provider and its models, keyed or not (agentd 0.20.0).
    /// `knownModels` is then only the models of those with a key.
    public let modelProviders: [ModelProvider]?
    /// The chat model's provider when it has no key yet: the app asks for one.
    public let needsModelKey: ProviderRef?

    enum CodingKeys: String, CodingKey {
        case desktop, busy, settings, version, stage, sessions, budget
        case lastSeq = "last_seq"
        case knownModels = "known_models"
        case shellTool = "shell_tool"
        case stageDetail = "stage_detail"
        case memoryV2 = "memory_v2"
        case modelProviders = "model_providers"
        case needsModelKey = "needs_model_key"
    }

    /// The provider that lists this model (or has it as its helper).
    public func provider(of model: String) -> ModelProvider? {
        modelProviders?.first { $0.models.contains(model) || $0.helper == model }
    }
}

/// A page of the event log, oldest first (GET /api/v1/events/page).
public struct EventPage: Decodable, Sendable, Equatable {
    public let events: [AgentEvent]
    /// Older events of those kinds remain.
    public let more: Bool
    /// The log's newest seq when the page was read: the live stream goes on after it.
    public let lastSeq: Int

    enum CodingKeys: String, CodingKey {
        case events, more
        case lastSeq = "last_seq"
    }

    public init(events: [AgentEvent], more: Bool, lastSeq: Int) {
        self.events = events
        self.more = more
        self.lastSeq = lastSeq
    }
}

struct ApprovalAnswer: Encodable, Equatable {
    let approved: Bool
    let text: String?
    var plan: [PlanEdit]? = nil
    /// "Allow for this task"; left out when not.
    var forTask: Bool? = nil

    enum CodingKeys: String, CodingKey {
        case approved, text, plan
        case forTask = "for_task"
    }
}

/// One step of a plan card's steps as the user approves them, when they
/// changed the plan: its words, its status, and the needs it kept (none
/// added: agentd refuses a need the card didn't have). nil fields are left out.
public struct PlanEdit: Encodable, Sendable, Equatable {
    public var step: String
    public var status: String?
    public var site: String?
    public var app: String?
    public var shell: Bool?
    public var keys: [String]?
    public var commands: [String]?
    public var install: String?

    public init(
        step: String, status: String? = nil, site: String? = nil, app: String? = nil, shell: Bool? = nil,
        keys: [String]? = nil, commands: [String]? = nil, install: String? = nil
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
}

public struct AgentSettings: Codable, Sendable, Equatable {
    public var model: String
    public var allowActions: Bool
    /// Eligible shell actions and schedules can run without waiting for the user.
    public var autoApprove: Bool
    public let autoApproveVersion: String?

    enum CodingKeys: String, CodingKey {
        case model
        case allowActions = "allow_actions"
        case autoApprove = "auto_approve"
        case autoApproveVersion = "auto_approve_version"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        model = try container.decode(String.self, forKey: .model)
        allowActions = try container.decode(Bool.self, forKey: .allowActions)
        autoApprove = try container.decodeIfPresent(Bool.self, forKey: .autoApprove) ?? false
        autoApproveVersion = try container.decodeIfPresent(String.self, forKey: .autoApproveVersion)
    }
}

public enum AgentError: LocalizedError, Equatable {
    case http(Int, String)

    public var errorDescription: String? {
        switch self {
        case .http(let code, let message): "agentd answered \(code): \(message)"
        }
    }

    public var statusCode: Int {
        switch self {
        case .http(let code, _): code
        }
    }
}

/// Talks to agentd on the desktop, e.g. https://neo-agent.tail57aeb.ts.net.
public struct AgentClient: Sendable {
    public let baseURL: URL
    let session: URLSession

    public init(host: String, session: URLSession) {
        self.init(baseURL: URL(string: "https://\(host)")!, session: session)
    }

    public init(baseURL: URL, session: URLSession) {
        self.baseURL = baseURL
        self.session = session
    }

    // MARK: Reading

    public func status() async throws -> AgentStatus {
        try await get("/api/v1/status")
    }

    /// The live event stream, starting after `after`, of `kinds` only (all
    /// when empty). Ends when the connection drops.
    public func events(after: Int, kinds: Set<String> = []) -> AsyncThrowingStream<AgentEvent, Error> {
        var query = [URLQueryItem(name: "after", value: String(after))]
        if !kinds.isEmpty { query.append(URLQueryItem(name: "kinds", value: kinds.sorted().joined(separator: ","))) }
        let url = baseURL.appending(path: "/api/v1/events").appending(queryItems: query)
        let session = session
        return AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    var request = URLRequest(url: url)
                    request.setValue("text/event-stream", forHTTPHeaderField: "Accept")
                    // agentd sends a keep-alive every 15 s; this is the idle limit.
                    request.timeoutInterval = 60
                    let (bytes, response) = try await session.bytes(for: request)
                    try Self.check(response, Data())
                    for try await line in bytes.lines {
                        if let event = try SSE.event(fromLine: line) {
                            continuation.yield(event)
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// Up to `limit` events of `kinds` before `before` (the newest when nil),
    /// oldest first: the chat a page at a time. With `session`, that
    /// conversation's events and the global ones (agentd 0.16.0). agentd
    /// before 0.15.1 answers 404.
    public func eventPage(
        before: Int?, limit: Int, kinds: Set<String>, session conversation: String? = nil
    ) async throws -> EventPage {
        var query = [
            URLQueryItem(name: "limit", value: String(limit)),
            URLQueryItem(name: "kinds", value: kinds.sorted().joined(separator: ",")),
        ]
        if let before { query.append(URLQueryItem(name: "before", value: String(before))) }
        if let conversation { query.append(URLQueryItem(name: "session", value: conversation)) }
        let url = baseURL.appending(path: "/api/v1/events/page").appending(queryItems: query)
        let (data, response) = try await session.data(from: url)
        try Self.check(response, data)
        return try JSONDecoder().decode(EventPage.self, from: data)
    }

    /// Image bytes for paths like /api/v1/images/<id> (thumbnails, screenshots).
    public func imageData(path: String) async throws -> Data {
        let (data, response) = try await session.data(from: baseURL.appending(path: path))
        try Self.check(response, data)
        return data
    }

    // MARK: Commands

    /// Sends a message to `main` with this device's time zone, so the agent
    /// knows the user's date ("yesterday"). It starts a task, or (agentd
    /// 0.16.0) queues behind the one running; an older agentd answers 409
    /// while another task runs.
    @discardableResult
    public func send(_ text: String, timeZone: TimeZone = .current) async throws -> Sent {
        try await post("/api/v1/messages", body: NewMessage(text, timeZone))
    }

    /// `text`: what the user changed the card's text to, on a card that takes
    /// an edit; `plan`: their version of a plan card's steps. Each only when
    /// they changed it.
    /// `forTask`: "Allow for this task", the card's kind of action without
    /// asking until the task ends.
    public func answer(
        approval id: String, approved: Bool, text: String? = nil, plan: [PlanEdit]? = nil, forTask: Bool = false
    ) async throws {
        let _: [String: Bool] = try await post(
            "/api/v1/approvals/\(id)",
            body: ApprovalAnswer(approved: approved, text: text, plan: plan, forTask: forTask ? true : nil))
    }

    /// "I'm done" (true) or "Skip" (false) on an action-needed card; starts a
    /// new task, or queues it in the card's conversation.
    @discardableResult
    public func resolve(action id: String, done: Bool) async throws -> Sent {
        try await post("/api/v1/actions/\(id)", body: ["choice": done ? "done" : "skip"])
    }

    /// Stops `main`'s task (agentd 0.16.0 also drops its queued messages).
    public func stop() async throws {
        try await perform("POST", "/api/v1/stop", body: [String: String]())
    }

    /// Registers this device for notifications: agentd stores the token and
    /// sends to it over APNs. `topic` is the app's bundle id; `environment` is
    /// the APNs servers the token is for (sandbox for Xcode installs,
    /// production for TestFlight or a notarized build). 400 when this desktop
    /// has no APNs key (notifications to the apps aren't set up).
    public func registerDevice(token: String, name: String, environment: String, topic: String) async throws -> String {
        struct Registration: Decodable { let id: String }
        let registration: Registration = try await post(
            "/api/v1/devices", body: ["token": token, "name": name, "environment": environment, "topic": topic])
        return registration.id
    }

    public func unregisterDevice(id: String) async throws {
        let _: [String: Bool] = try await send("DELETE", "/api/v1/devices/\(id)", body: [String: String]())
    }

    public func updateSettings(model: String? = nil, allowActions: Bool? = nil) async throws -> AgentSettings {
        struct Update: Encodable {
            let model: String?
            let allowActions: Bool?
            enum CodingKeys: String, CodingKey {
                case model
                case allowActions = "allow_actions"
            }
        }
        return try await send("PUT", "/api/v1/settings", body: Update(model: model, allowActions: allowActions))
    }

    public func setAutoApprove(_ enabled: Bool, expectedVersion: String?) async throws -> AgentSettings {
        if !enabled, expectedVersion == nil {
            return try await send("PUT", "/api/v1/settings", body: ["auto_approve": false])
        }
        struct Update: Encodable {
            let enabled: Bool
            let expectedVersion: String?
            enum CodingKeys: String, CodingKey {
                case enabled
                case expectedVersion = "expected_version"
            }
        }
        return try await send(
            "PUT", "/api/v1/settings/auto-approve", body: Update(enabled: enabled, expectedVersion: expectedVersion))
    }

    /// What the agent remembers across conversations.
    public func memories() async throws -> [AgentMemory] {
        struct Reply: Decodable { let memories: [AgentMemory] }
        let reply: Reply = try await get("/api/v1/memory")
        return reply.memories
    }

    public func forget(memory id: Int) async throws {
        let _: Ignored = try await send("DELETE", "/api/v1/memory/\(id)", body: [String: String]())
    }

    /// Vouches for a memory saved after the agent read outside content: it
    /// becomes something the user said.
    public func confirm(memory id: Int) async throws {
        let _: Ignored = try await post("/api/v1/memory/\(id)/confirm", body: [String: String]())
    }

    /// Memory v2: memories by status (default active), kind and full text.
    public func memories(statuses: [String], kinds: [String] = [], query: String? = nil) async throws -> [AgentMemory] {
        struct Reply: Decodable { let memories: [AgentMemory] }
        var items = [URLQueryItem(name: "status", value: statuses.joined(separator: ","))]
        if !kinds.isEmpty { items.append(URLQueryItem(name: "kind", value: kinds.joined(separator: ","))) }
        if let query, !query.isEmpty { items.append(URLQueryItem(name: "q", value: query)) }
        let url = baseURL.appending(path: "/api/v1/memory").appending(queryItems: items)
        let (data, response) = try await session.data(from: url)
        try Self.check(response, data)
        return try JSONDecoder().decode(Reply.self, from: data).memories
    }

    /// Memory v2: brings back an expired or replaced memory.
    public func restore(memory id: Int) async throws {
        let _: Ignored = try await post("/api/v1/memory/\(id)/restore", body: [String: String]())
    }

    /// Memory v2: a user's edit. 409 when it changed meanwhile. `validUntil`:
    /// a `YYYY-MM-DD` date, or `.some(nil)` to clear it.
    public func edit(
        memory id: Int, text: String?, kind: String?, validUntil: String??, expectedVersion: Int
    ) async throws {
        struct Body: Encodable {
            let text: String?
            let kind: String?
            let validUntil: String??
            let expectedVersion: Int

            enum CodingKeys: String, CodingKey {
                case text, kind
                case validUntil = "valid_until"
                case expectedVersion = "expected_version"
            }

            func encode(to encoder: Encoder) throws {
                var c = encoder.container(keyedBy: CodingKeys.self)
                try c.encodeIfPresent(text, forKey: .text)
                try c.encodeIfPresent(kind, forKey: .kind)
                if let validUntil {
                    // .some(nil) clears the date: an explicit null.
                    try c.encode(validUntil, forKey: .validUntil)
                }
                try c.encode(expectedVersion, forKey: .expectedVersion)
            }
        }
        let _: Ignored = try await send(
            "PATCH", "/api/v1/memory/\(id)",
            body: Body(text: text, kind: kind, validUntil: validUntil, expectedVersion: expectedVersion))
    }

    /// Memory v2: every memory as JSON (to import elsewhere), or the active
    /// ones as Markdown (to read).
    public func exportMemory(markdown: Bool) async throws -> Data {
        let url = baseURL.appending(path: "/api/v1/memory/export")
            .appending(queryItems: [URLQueryItem(name: "format", value: markdown ? "markdown" : "json")])
        let (data, response) = try await session.data(from: url)
        try Self.check(response, data)
        return data
    }

    /// Memory v2: imports a JSON export (or a list of `{text, kind}`); it all
    /// waits for approval.
    public func importMemory(_ json: Data) async throws -> MemoryImport {
        var request = URLRequest(url: baseURL.appending(path: "/api/v1/memory/import"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = json
        let (data, response) = try await session.data(for: request)
        try Self.check(response, data)
        return try JSONDecoder().decode(MemoryImport.self, from: data)
    }

    /// Memory v2: consolidation's mode, model, cost this month and last run.
    public func consolidation() async throws -> ConsolidationStats {
        try await get("/api/v1/memory/consolidation")
    }

    /// Memory v2: consolidation's mode (off, observe, auto).
    public func setConsolidation(mode: String) async throws {
        let _: Ignored = try await send("PUT", "/api/v1/settings", body: ["memory_consolidation": mode])
    }

    /// Memory v2: tidy up now rather than 10 minutes after the last task.
    public func consolidateNow() async throws {
        let _: Ignored = try await post("/api/v1/memory/consolidate", body: [String: String]())
    }

    public func schedules() async throws -> [AgentSchedule] {
        struct Reply: Decodable { let schedules: [AgentSchedule] }
        let reply: Reply = try await get("/api/v1/schedules")
        return reply.schedules
    }

    public func cancel(schedule id: Int) async throws {
        let _: [String: Bool] = try await send("DELETE", "/api/v1/schedules/\(id)", body: [String: String]())
    }

    /// Pauses or resumes a schedule; returns it as it is now.
    public func setPaused(_ paused: Bool, schedule id: Int) async throws -> AgentSchedule {
        try await post("/api/v1/schedules/\(id)/\(paused ? "pause" : "resume")", body: [String: String]())
    }

    public func secrets() async throws -> [AgentSecret] {
        struct Reply: Decodable { let secrets: [AgentSecret] }
        let reply: Reply = try await get("/api/v1/secrets")
        return reply.secrets
    }

    /// Saves (or replaces) a key on the desktop. With `approval`, the task that
    /// asked for it carries on. The value travels only over the tailnet.
    public func saveSecret(name: String, value: String, label: String?, approval: String?) async throws -> SavedSecret {
        struct Body: Encodable {
            let value: String
            let label: String?
            let approval: String?
        }
        return try await send(
            "PUT", "/api/v1/secrets/\(name)", body: Body(value: value, label: label, approval: approval))
    }

    public func deleteSecret(name: String) async throws {
        let _: [String: Bool] = try await send("DELETE", "/api/v1/secrets/\(name)", body: [String: String]())
    }

    /// Saves a provider's key, pasted in the app (agentd 0.20.0), once the
    /// provider takes it: the next model call uses it, with no restart. 400
    /// when the provider refused it. The value travels only over the tailnet,
    /// isn't kept here, and never goes to the agent's commands.
    public func saveModelKey(provider: String, value: String) async throws -> SavedModelKey {
        try await send("PUT", "/api/v1/model-keys/\(provider)", body: ["value": value])
    }

    /// Removes the key pasted in an app; the deployment's, if any, takes
    /// over. 404 when none was saved in an app.
    public func deleteModelKey(provider: String) async throws {
        try await perform("DELETE", "/api/v1/model-keys/\(provider)")
    }

    // MARK: Plumbing

    func get<T: Decodable>(_ path: String) async throws -> T {
        let (data, response) = try await session.data(from: baseURL.appending(path: path))
        try Self.check(response, data)
        return try JSONDecoder().decode(T.self, from: data)
    }

    func post<T: Decodable, Body: Encodable>(_ path: String, body: Body) async throws -> T {
        try await send("POST", path, body: body)
    }

    func send<T: Decodable, Body: Encodable>(_ method: String, _ path: String, body: Body) async throws -> T {
        try JSONDecoder().decode(T.self, from: await perform(method, path, body: body))
    }

    /// A request whose answer may have no body (204).
    @discardableResult
    func perform(_ method: String, _ path: String, body: (any Encodable)? = nil) async throws -> Data {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = method
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        let (data, response) = try await session.data(for: request)
        try Self.check(response, data)
        return data
    }

    static func check(_ response: URLResponse, _ data: Data) throws {
        guard let http = response as? HTTPURLResponse else { return }
        guard (200..<300).contains(http.statusCode) else {
            // Only `error`: other fields (a 409's needs_model_key) needn't be strings.
            struct Problem: Decodable { let error: String }
            let message = (try? JSONDecoder().decode(Problem.self, from: data))?.error
            throw AgentError.http(http.statusCode, message ?? String(decoding: data, as: UTF8.self))
        }
    }
}
