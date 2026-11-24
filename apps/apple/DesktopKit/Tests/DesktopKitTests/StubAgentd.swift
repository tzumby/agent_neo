import Foundation

@testable import AgentAPI

/// A stand-in agentd for the store's tests: fixed answers by method and path,
/// and a record of what was asked. Each one has its own host, so tests run
/// side by side. The event stream answers and stays open, as agentd's does.
final class StubAgentd: @unchecked Sendable {
    struct Reply {
        var status = 200
        var body = "{}"
        /// The event stream: the body, then the connection stays open.
        var stream = false
    }

    let host = "agentd-\(UUID().uuidString.lowercased()).test"
    private let lock = NSLock()
    private var routes: [String: @Sendable (URLRequest) -> Reply] = [:]
    private var asked: [URLRequest] = []

    init() {
        Self.lock.withLock { Self.stubs[host] = self }
    }

    deinit {
        Self.lock.withLock { Self.stubs[host] = nil }
    }

    var client: AgentClient {
        AgentClient(baseURL: URL(string: "http://\(host)")!, session: Self.session)
    }

    /// Answers `"GET /api/v1/sessions"` (the query aside).
    func on(_ route: String, _ reply: @escaping @Sendable (URLRequest) -> Reply) {
        lock.withLock { routes[route] = reply }
    }

    func on(_ route: String, status: Int = 200, _ body: String = "{}") {
        on(route) { _ in Reply(status: status, body: body) }
    }

    /// The requests made so far, as "GET /api/v1/sessions?archived=1".
    var requests: [String] {
        lock.withLock {
            asked.map { request in
                let url = request.url!
                return "\(request.httpMethod ?? "GET") \(url.path())\(url.query().map { "?\($0)" } ?? "")"
            }
        }
    }

    func count(_ prefix: String) -> Int {
        requests.filter { $0.hasPrefix(prefix) }.count
    }

    fileprivate func reply(to request: URLRequest) -> Reply {
        lock.withLock {
            asked.append(request)
            let route = "\(request.httpMethod ?? "GET") \(request.url!.path())"
            return routes[route]?(request) ?? Reply(status: 404, body: #"{"error":"not found"}"#)
        }
    }

    private static let lock = NSLock()
    nonisolated(unsafe) private static var stubs: [String: StubAgentd] = [:]

    fileprivate static func stub(for host: String?) -> StubAgentd? {
        lock.withLock { host.flatMap { stubs[$0] } }
    }

    private static let session: URLSession = {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        return URLSession(configuration: configuration)
    }()
}

private final class StubProtocol: URLProtocol {
    override class func canInit(with request: URLRequest) -> Bool {
        StubAgentd.stub(for: request.url?.host()) != nil
    }

    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        guard let url = request.url, let stub = StubAgentd.stub(for: url.host()) else {
            client?.urlProtocol(self, didFailWithError: URLError(.cannotFindHost))
            return
        }
        let reply = stub.reply(to: request)
        let response = HTTPURLResponse(
            url: url, statusCode: reply.status, httpVersion: "HTTP/1.1",
            headerFields: ["Content-Type": reply.stream ? "text/event-stream" : "application/json"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data(reply.body.utf8))
        if !reply.stream { client?.urlProtocolDidFinishLoading(self) }
    }

    override func stopLoading() {}
}

/// Waits until `condition` holds (up to `limit`).
@MainActor
func waitUntil(_ limit: Duration = .seconds(5), _ condition: () -> Bool) async throws {
    let deadline = ContinuousClock.now.advanced(by: limit)
    while !condition() {
        if ContinuousClock.now >= deadline { throw WaitTimedOut() }
        try await Task.sleep(for: .milliseconds(10))
    }
}

struct WaitTimedOut: Error {}

/// Server-Sent Events lines for agentd's events.
func sse(_ events: [(seq: Int, kind: String, data: String)]) -> String {
    events.map { #"data: {"seq":\#($0.seq),"ts":"2026-11-24T10:00:00Z","kind":"\#($0.kind)","data":\#($0.data)}"# + "\n\n" }
        .joined()
}
