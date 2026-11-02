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
    /// active, pending, superseded or expired.
    public let status: String?
    /// user_stated, agent_inferred or imported.
    public let trust: String?
    /// Why it waits: tainted_rule, conflict, proposed or imported.
    public let pendingReason: String?
    /// For edits: the version the edit is based on.
    public let version: Int?
    /// Unix seconds; after it the memory expires.
    public let validUntil: Int?
    /// The task that saved it (to open that conversation).
    public let sourceRun: String?
    /// Who wrote it first: agent, user or consolidator.
    public let origin: String?
    /// For a proposal: what approving it replaces.
    public let replaces: [Replaced]?

    public struct Replaced: Decodable, Sendable, Equatable {
        public let id: Int
        public let text: String
    }

    enum CodingKeys: String, CodingKey {
        case id, text, trusted, created, kind, status, trust, version, origin, replaces
        case pendingReason = "pending_reason"
        case validUntil = "valid_until"
        case sourceRun = "source_run"
    }

    public init(
        id: Int, text: String, trusted: Bool, created: String, kind: String? = nil, status: String? = nil,
        trust: String? = nil, pendingReason: String? = nil, version: Int? = nil, validUntil: Int? = nil,
        sourceRun: String? = nil, origin: String? = nil, replaces: [Replaced]? = nil
    ) {
        self.id = id
        self.text = text
        self.trusted = trusted
        self.created = created
        self.kind = kind
        self.status = status
        self.trust = trust
        self.pendingReason = pendingReason
        self.version = version
        self.validUntil = validUntil
        self.sourceRun = sourceRun
        self.origin = origin
        self.replaces = replaces
    }

    public var isPending: Bool { status == "pending" }
    /// Expired or replaced: it can be restored.
    public var isGone: Bool { status == "expired" || status == "superseded" }
    /// Saved after the agent read outside content: a hint, not an instruction.
    public var isInferred: Bool { trust.map { $0 == "agent_inferred" } ?? !trusted }
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
        public let proposed: Int
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

    enum CodingKeys: String, CodingKey {
        case desktop, busy, settings, version, stage
        case lastSeq = "last_seq"
        case knownModels = "known_models"
        case shellTool = "shell_tool"
        case stageDetail = "stage_detail"
        case memoryV2 = "memory_v2"
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
}

public struct AgentSettings: Codable, Sendable, Equatable {
    public var model: String
    public var allowActions: Bool

    enum CodingKeys: String, CodingKey {
        case model
        case allowActions = "allow_actions"
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

/// Talks to agentd on a cloud desktop, e.g. https://cloud-desktop-dev.tail57aeb.ts.net.
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
    /// oldest first: the chat a page at a time. agentd before 0.15.1 answers 404.
    public func eventPage(before: Int?, limit: Int, kinds: Set<String>) async throws -> EventPage {
        var query = [
            URLQueryItem(name: "limit", value: String(limit)),
            URLQueryItem(name: "kinds", value: kinds.sorted().joined(separator: ",")),
        ]
        if let before { query.append(URLQueryItem(name: "before", value: String(before))) }
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

    /// Starts a task. Throws AgentError.http(409, …) while another one runs.
    @discardableResult
    /// Sends a message with this device's time zone, so the agent knows the
    /// user's date ("yesterday").
    public func send(_ text: String, timeZone: TimeZone = .current) async throws -> String {
        let message = NewMessage(text: text, timezone: timeZone.identifier, utcOffset: timeZone.secondsFromGMT())
        let reply: [String: String] = try await post("/api/v1/messages", body: message)
        return reply["task"] ?? ""
    }

    private struct NewMessage: Encodable {
        let text: String
        let timezone: String
        let utcOffset: Int

        enum CodingKeys: String, CodingKey {
            case text, timezone
            case utcOffset = "utc_offset"
        }
    }

    public func answer(approval id: String, approved: Bool) async throws {
        let _: [String: Bool] = try await post("/api/v1/approvals/\(id)", body: ["approved": approved])
    }

    /// "I'm done" (true) or "Skip" (false) on an action-needed card; starts a new task.
    @discardableResult
    public func resolve(action id: String, done: Bool) async throws -> String {
        let reply: [String: String] = try await post("/api/v1/actions/\(id)", body: ["choice": done ? "done" : "skip"])
        return reply["task"] ?? ""
    }

    /// Registers this device for push notifications (the desktop passes the
    /// token on to the push relay). 503 when push isn't set up there.
    public func registerDevice(token: String, name: String) async throws {
        let _: [String: Bool] = try await post("/api/v1/devices", body: ["token": token, "name": name])
    }

    public func stop() async throws {
        let _: [String: Bool] = try await post("/api/v1/stop", body: [String: String]())
    }

    public func update(settings: AgentSettings) async throws -> AgentSettings {
        try await send("PUT", "/api/v1/settings", body: settings)
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

    /// Vouches for a memory saved after the agent read outside content (memory
    /// v2: approves a pending one too).
    public func confirm(memory id: Int) async throws {
        let _: Ignored = try await post("/api/v1/memory/\(id)/confirm", body: [String: String]())
    }

    /// Memory v2: memories by status (default active and pending), kind and
    /// full text.
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

    /// Memory v2: approves or rejects every pending memory (of one reason).
    public func approveAll(reason: String?) async throws {
        let _: Ignored = try await post("/api/v1/memory/approve-all", body: ["pending_reason": reason])
    }

    public func rejectAll(reason: String?) async throws {
        let _: Ignored = try await post("/api/v1/memory/reject-all", body: ["pending_reason": reason])
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

    /// Memory v2: turns down a memory waiting for approval.
    public func reject(memory id: Int) async throws {
        let _: Ignored = try await post("/api/v1/memory/\(id)/reject", body: [String: String]())
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
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        let (data, response) = try await session.data(for: request)
        try Self.check(response, data)
        return try JSONDecoder().decode(T.self, from: data)
    }

    static func check(_ response: URLResponse, _ data: Data) throws {
        guard let http = response as? HTTPURLResponse else { return }
        guard (200..<300).contains(http.statusCode) else {
            let message = (try? JSONDecoder().decode([String: String].self, from: data))?["error"]
            throw AgentError.http(http.statusCode, message ?? String(decoding: data, as: UTF8.self))
        }
    }
}
