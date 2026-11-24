import Foundation

/// One entry of agentd's event log (GET /api/v1/events). `seq` only grows, so
/// a client resumes with `after: lastSeq`.
public struct AgentEvent: Decodable, Sendable, Equatable {
    public let seq: Int
    public let ts: String
    public let kind: String
    public let data: JSONValue

    public init(seq: Int, ts: String = "", kind: String, data: JSONValue) {
        self.seq = seq
        self.ts = ts
        self.kind = kind
        self.data = data
    }
}

/// Server-Sent Events: the JSON after `data:`; comments and other fields are skipped.
enum SSE {
    static func event(fromLine line: String) throws -> AgentEvent? {
        guard line.hasPrefix("data:") else { return nil }
        let json = line.dropFirst(5).trimmingCharacters(in: .whitespaces)
        guard !json.isEmpty else { return nil }
        return try JSONDecoder().decode(AgentEvent.self, from: Data(json.utf8))
    }
}
