import Foundation

/// GET /api/v1/status from agentd (through tailnet-proxy).
public struct AgentStatus: Decodable, Sendable, Equatable {
    public struct Settings: Decodable, Sendable, Equatable {
        public let model: String
        public let allowActions: Bool

        enum CodingKeys: String, CodingKey {
            case model
            case allowActions = "allow_actions"
        }
    }

    public let desktop: String
    public let busy: Bool
    public let settings: Settings
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

public enum AgentError: LocalizedError {
    case http(Int, String)

    public var errorDescription: String? {
        switch self {
        case .http(let code, let message): "agentd answered \(code): \(message)"
        }
    }
}

/// Talks to agentd on a cloud desktop, e.g. https://cloud-desktop-dev.tail57aeb.ts.net.
public struct AgentClient: Sendable {
    public let baseURL: URL
    let session: URLSession

    public init(host: String, session: URLSession) {
        baseURL = URL(string: "https://\(host)")!
        self.session = session
    }

    public func status() async throws -> AgentStatus {
        try await get("/api/v1/status")
    }

    func get<T: Decodable>(_ path: String) async throws -> T {
        let (data, response) = try await session.data(from: baseURL.appending(path: path))
        try check(response, data)
        return try JSONDecoder().decode(T.self, from: data)
    }

    func check(_ response: URLResponse, _ data: Data) throws {
        guard let http = response as? HTTPURLResponse else { return }
        guard (200..<300).contains(http.statusCode) else {
            let message = (try? JSONDecoder().decode([String: String].self, from: data))?["error"]
            throw AgentError.http(http.statusCode, message ?? String(decoding: data, as: UTF8.self))
        }
    }
}
