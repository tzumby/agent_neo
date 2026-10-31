import Foundation

/// GET /api/v1/status from agentd (through tailnet-proxy).
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
