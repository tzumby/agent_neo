import Foundation

/// A conversation (`session` in agentd's API, from 0.16.0): a chat with its
/// own history, browser window and cards. Memory, keys, schedules and
/// settings stay shared by all of them.
public struct AgentSession: Decodable, Sendable, Equatable, Identifiable, Hashable {
    /// The first conversation, with everything from before conversations. It
    /// can be renamed and archived, never deleted.
    public static let main = "main"

    public enum State: String, Sendable, Hashable {
        /// A task runs.
        case working
        /// A task is parked on a card.
        case waiting
        /// It has queued messages and this month's budget is used up.
        case paused
        /// It has queued messages waiting for a free slot.
        case queued
        case idle
    }

    public let id: String
    /// Empty until the first message.
    public var title: String
    public let created: Date
    public let updated: Date
    public var archived: Bool
    public let state: State
    /// What it waits on the user for: approval, secret or action (nil: nothing).
    public let needsYou: String?
    /// Messages accepted but not started yet, in the order they'll start.
    public let queued: [QueuedMessage]
    /// Its newest history entry, redacted.
    public let last: Entry?

    public struct Entry: Decodable, Sendable, Equatable, Hashable {
        public let role: String
        /// The first 160 characters.
        public let text: String
        public let ts: Date?

        public init(role: String, text: String, ts: Date?) {
            self.role = role
            self.text = text
            self.ts = ts
        }

        enum CodingKeys: String, CodingKey { case role, text, ts }

        public init(from decoder: any Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            role = try c.decodeIfPresent(String.self, forKey: .role) ?? "assistant"
            text = try c.decodeIfPresent(String.self, forKey: .text) ?? ""
            ts = try c.decodeIfPresent(Double.self, forKey: .ts).map(Date.init(timeIntervalSince1970:))
        }
    }

    enum CodingKeys: String, CodingKey {
        case id, title, created, updated, archived, state, queued, last
        case needsYou = "needs_you"
    }

    public init(
        id: String, title: String, created: Date = .now, updated: Date = .now, archived: Bool = false,
        state: State = .idle, needsYou: String? = nil, queued: [QueuedMessage] = [], last: Entry? = nil
    ) {
        self.id = id
        self.title = title
        self.created = created
        self.updated = updated
        self.archived = archived
        self.state = state
        self.needsYou = needsYou
        self.queued = queued
        self.last = last
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        title = try c.decodeIfPresent(String.self, forKey: .title) ?? ""
        let created = try c.decodeIfPresent(Double.self, forKey: .created) ?? 0
        self.created = Date(timeIntervalSince1970: created)
        updated = Date(timeIntervalSince1970: try c.decodeIfPresent(Double.self, forKey: .updated) ?? created)
        archived = try c.decodeIfPresent(Bool.self, forKey: .archived) ?? false
        // A state this app doesn't know yet reads as idle.
        state = State(rawValue: try c.decodeIfPresent(String.self, forKey: .state) ?? "") ?? .idle
        needsYou = try c.decodeIfPresent(String.self, forKey: .needsYou)
        queued = try c.decodeIfPresent([QueuedMessage].self, forKey: .queued) ?? []
        last = try c.decodeIfPresent(Entry.self, forKey: .last)
    }

    /// Its title, or "New conversation" before the first message.
    public var displayTitle: String {
        let title = title.trimmingCharacters(in: .whitespacesAndNewlines)
        return title.isEmpty ? "New conversation" : title
    }

    /// Nothing in it yet: a new conversation can reuse it instead of adding another.
    public var isBlank: Bool {
        title.isEmpty && last == nil && queued.isEmpty && state == .idle && needsYou == nil
    }
}

/// A message accepted while its conversation was busy, waiting its turn.
public struct QueuedMessage: Decodable, Sendable, Equatable, Identifiable, Hashable {
    public let id: String
    public let text: String
    /// Who sent it (the device or browser).
    public let by: String?
    public let created: Date

    public init(id: String, text: String, by: String? = nil, created: Date = .now) {
        self.id = id
        self.text = text
        self.by = by
        self.created = created
    }

    enum CodingKeys: String, CodingKey { case id, text, by, created }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        text = try c.decodeIfPresent(String.self, forKey: .text) ?? ""
        by = try c.decodeIfPresent(String.self, forKey: .by)
        created = Date(timeIntervalSince1970: try c.decodeIfPresent(Double.self, forKey: .created) ?? 0)
    }
}

/// GET /api/v1/sessions.
public struct SessionList: Decodable, Sendable, Equatable {
    /// Most recently updated first.
    public let sessions: [AgentSession]
    /// How many conversations can work at the same moment.
    public let runningLimit: Int

    enum CodingKeys: String, CodingKey {
        case sessions
        case runningLimit = "running_limit"
    }

    public init(sessions: [AgentSession], runningLimit: Int = 3) {
        self.sessions = sessions
        self.runningLimit = runningLimit
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        sessions = try c.decodeIfPresent([AgentSession].self, forKey: .sessions) ?? []
        runningLimit = try c.decodeIfPresent(Int.self, forKey: .runningLimit) ?? 3
    }
}

/// What sending a message did: it started a task, or it waits its turn.
public enum Sent: Decodable, Sendable, Equatable {
    case started(task: String)
    /// `position`: 1 is next.
    case queued(id: String, position: Int)

    enum CodingKeys: String, CodingKey { case task, queued }

    private struct Queued: Decodable {
        let id: String
        let position: Int?
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        if let queued = try c.decodeIfPresent(Queued.self, forKey: .queued) {
            self = .queued(id: queued.id, position: queued.position ?? 1)
        } else {
            self = .started(task: try c.decodeIfPresent(String.self, forKey: .task) ?? "")
        }
    }
}

/// How many conversations work, wait and have messages queued (in GET /status).
public struct SessionCounts: Decodable, Sendable, Equatable {
    public let working: Int
    public let waiting: Int
    public let queued: Int
    public let runningLimit: Int

    enum CodingKeys: String, CodingKey {
        case working, waiting, queued
        case runningLimit = "running_limit"
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        working = try c.decodeIfPresent(Int.self, forKey: .working) ?? 0
        waiting = try c.decodeIfPresent(Int.self, forKey: .waiting) ?? 0
        queued = try c.decodeIfPresent(Int.self, forKey: .queued) ?? 0
        runningLimit = try c.decodeIfPresent(Int.self, forKey: .runningLimit) ?? 3
    }
}

/// The monthly budget and what this month has cost so far.
public struct Budget: Decodable, Sendable, Equatable {
    /// nil: no budget set.
    public let monthlyUsd: Double?
    public let spent: Double
    /// spent / monthlyUsd (nil without a budget).
    public let fraction: Double?
    /// Used up: new tasks wait until it's raised or the month turns over.
    public let paused: Bool
    /// "2026-11".
    public let month: String?

    enum CodingKeys: String, CodingKey {
        case spent, fraction, paused, month
        case monthlyUsd = "monthly_usd"
    }

    public init(monthlyUsd: Double?, spent: Double, fraction: Double?, paused: Bool, month: String? = nil) {
        self.monthlyUsd = monthlyUsd
        self.spent = spent
        self.fraction = fraction
        self.paused = paused
        self.month = month
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        monthlyUsd = try c.decodeIfPresent(Double.self, forKey: .monthlyUsd)
        spent = try c.decodeIfPresent(Double.self, forKey: .spent) ?? 0
        fraction = try c.decodeIfPresent(Double.self, forKey: .fraction)
        paused = try c.decodeIfPresent(Bool.self, forKey: .paused) ?? false
        month = try c.decodeIfPresent(String.self, forKey: .month)
    }
}

// MARK: - Conversations

extension AgentClient {
    /// The conversations, most recently updated first; with `archived`, the
    /// archived ones instead. agentd before 0.16.0 answers 404.
    public func sessions(archived: Bool = false) async throws -> SessionList {
        let url = baseURL.appending(path: "/api/v1/sessions")
            .appending(queryItems: archived ? [URLQueryItem(name: "archived", value: "1")] : [])
        let (data, response) = try await session.data(from: url)
        try Self.check(response, data)
        return try JSONDecoder().decode(SessionList.self, from: data)
    }

    /// One conversation; 404 when it's unknown or deleted.
    public func session(_ id: String) async throws -> AgentSession {
        try await get("/api/v1/sessions/\(id)")
    }

    /// A new, empty conversation.
    public func createSession(title: String? = nil) async throws -> AgentSession {
        struct Body: Encodable { let title: String? }
        return try await post("/api/v1/sessions", body: Body(title: title))
    }

    /// Renames, archives or unarchives it. 409 when archiving one that's
    /// working or waiting.
    public func updateSession(_ id: String, title: String? = nil, archived: Bool? = nil) async throws -> AgentSession {
        struct Body: Encodable {
            let title: String?
            let archived: Bool?
        }
        return try await send("PATCH", "/api/v1/sessions/\(id)", body: Body(title: title, archived: archived))
    }

    /// Deletes its history and queued messages (its runs stay for usage).
    /// 409 while it's working or waiting, and for `main`.
    public func deleteSession(_ id: String) async throws {
        try await perform("DELETE", "/api/v1/sessions/\(id)")
    }

    /// Sends a message to a conversation: it starts a task, or queues behind
    /// the one running (404: the conversation is archived or deleted).
    @discardableResult
    public func send(_ text: String, session id: String, timeZone: TimeZone = .current) async throws -> Sent {
        try await post("/api/v1/sessions/\(id)/messages", body: NewMessage(text, timeZone))
    }

    /// Takes a queued message out of the queue. 404 when it's gone already
    /// (started or removed).
    public func unqueue(_ id: String, session: String) async throws {
        try await perform("DELETE", "/api/v1/sessions/\(session)/queue/\(id)")
    }

    /// Stops the conversation's task (or gives up on its card) and drops its
    /// queued messages.
    public func stop(session id: String) async throws {
        try await perform("POST", "/api/v1/sessions/\(id)/stop", body: [String: String]())
    }
}

/// POST /messages' body.
struct NewMessage: Encodable {
    let text: String
    let timezone: String
    let utcOffset: Int

    init(_ text: String, _ timeZone: TimeZone) {
        self.text = text
        timezone = timeZone.identifier
        utcOffset = timeZone.secondsFromGMT()
    }

    enum CodingKeys: String, CodingKey {
        case text, timezone
        case utcOffset = "utc_offset"
    }
}
