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
    /// active or pending (the list shows only these by default).
    public let status: String?
    /// user_stated, agent_inferred or imported.
    public let trust: String?
    /// Why it waits: tainted_rule, conflict, proposed or imported.
    public let pendingReason: String?

    enum CodingKeys: String, CodingKey {
        case id, text, trusted, created, kind, status, trust
        case pendingReason = "pending_reason"
    }

    public init(
        id: Int, text: String, trusted: Bool, created: String, kind: String? = nil, status: String? = nil,
        trust: String? = nil, pendingReason: String? = nil
    ) {
        self.id = id
        self.text = text
        self.trusted = trusted
        self.created = created
        self.kind = kind
        self.status = status
        self.trust = trust
        self.pendingReason = pendingReason
    }

    public var isPending: Bool { status == "pending" }
    /// Saved after the agent read outside content: a hint, not an instruction.
    public var isInferred: Bool { trust.map { $0 == "agent_inferred" } ?? !trusted }
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

    public var repeats: Bool { cron != nil }

    enum CodingKeys: String, CodingKey {
        case id, name, prompt, cron, timezone, description, paused
        case nextRun = "next_run"
        case lastRun = "last_run"
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

    enum CodingKeys: String, CodingKey {
        case desktop, busy, settings, version
        case lastSeq = "last_seq"
        case knownModels = "known_models"
        case shellTool = "shell_tool"
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

    /// The live event stream, starting after `after`. Ends when the connection drops.
    public func events(after: Int) -> AsyncThrowingStream<AgentEvent, Error> {
        let url = baseURL.appending(path: "/api/v1/events")
            .appending(queryItems: [URLQueryItem(name: "after", value: String(after))])
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
