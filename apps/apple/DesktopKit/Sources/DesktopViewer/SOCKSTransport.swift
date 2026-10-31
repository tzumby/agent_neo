import Foundation
import Network
import RFB
import Synchronization

/// A SOCKS5 proxy with username/password auth, e.g. the in-app Tailscale
/// node's loopback proxy into the tailnet (user "tsnet").
public struct SOCKSProxy: Sendable, Hashable {
    public var host: String
    public var port: UInt16
    public var username: String
    public var password: String

    public init(host: String, port: UInt16, username: String, password: String) {
        self.host = host
        self.port = port
        self.username = username
        self.password = password
    }
}

/// Where the take-over view connects: the desktop's VNC port on the tailnet,
/// through the in-app node's proxy.
public struct DesktopEndpoint: Sendable, Hashable {
    public var proxy: SOCKSProxy
    public var host: String
    public var port: UInt16

    public init(proxy: SOCKSProxy, host: String, port: UInt16 = 5900) {
        self.proxy = proxy
        self.host = host
        self.port = port
    }

    public func connect() async throws -> SOCKSTransport {
        let transport = SOCKSTransport(proxy: proxy, host: host, port: port)
        try await transport.connect()
        return transport
    }
}

/// The VNC byte stream to `host:port` through a SOCKS5 proxy (RFC 1928 with
/// RFC 1929 auth). After `connect()` it behaves like a plain TCP stream.
public final class SOCKSTransport: RFBTransport {
    private let connection: NWConnection
    private let queue = DispatchQueue(label: "desktop.socks", qos: .userInteractive)
    private let proxy: SOCKSProxy
    private let host: String
    private let port: UInt16
    private let waiter = Mutex<CheckedContinuation<Void, any Error>?>(nil)
    public let executor: (any TaskExecutor)?

    public init(proxy: SOCKSProxy, host: String, port: UInt16) {
        self.proxy = proxy
        self.host = host
        self.port = port
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        connection = NWConnection(
            host: NWEndpoint.Host(proxy.host), port: NWEndpoint.Port(rawValue: proxy.port) ?? 1080,
            using: NWParameters(tls: nil, tcp: tcp))
        executor = QueueExecutor(queue)
    }

    deinit {
        connection.cancel()
    }

    /// Connects to the proxy and asks it for `host:port`.
    public func connect() async throws {
        try await withTaskCancellationHandler {
            try await ready()
            try await handshake()
        } onCancel: {
            connection.cancel()
        }
    }

    private func ready() async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, any Error>) in
            waiter.withLock { $0 = continuation }
            connection.stateUpdateHandler = { [weak self] state in
                let outcome: Result<Void, any Error>
                switch state {
                case .ready: outcome = .success(())
                case .failed(let error), .waiting(let error):
                    self?.connection.cancel()
                    outcome = .failure(RFBError.transport("proxy: \(error)"))
                case .cancelled: outcome = .failure(RFBError.connectionClosed)
                default: return
                }
                self?.waiter.withLock { w in
                    defer { w = nil }
                    return w
                }?.resume(with: outcome)
            }
            connection.start(queue: queue)
        }
    }

    private func handshake() async throws {
        // Greeting: version 5, one method, 2 = username/password.
        send(Data([5, 1, 2]))
        guard try await read(2) == Data([5, 2]) else { throw RFBError.transport("proxy refused password auth") }
        let user = Data(proxy.username.utf8)
        let password = Data(proxy.password.utf8)
        guard user.count < 256, password.count < 256 else { throw RFBError.transport("proxy credentials too long") }
        send(Data([1, UInt8(user.count)]) + user + Data([UInt8(password.count)]) + password)
        guard try await read(2).last == 0 else { throw RFBError.transport("proxy rejected the credentials") }

        // CONNECT to an IPv4 address or a name the proxy resolves.
        var request = Data([5, 1, 0])
        if let ipv4 = IPv4Address(host) {
            request.append(1)
            request.append(ipv4.rawValue)
        } else {
            let name = Data(host.utf8)
            guard name.count < 256 else { throw RFBError.transport("host name too long") }
            request.append(contentsOf: [3, UInt8(name.count)])
            request.append(name)
        }
        request.append(contentsOf: [UInt8(port >> 8), UInt8(port & 0xFF)])
        send(request)

        let reply = try await read(4)
        guard reply[reply.startIndex + 1] == 0 else {
            throw RFBError.transport("proxy couldn't connect to \(host):\(port) (SOCKS error \(reply[reply.startIndex + 1]))")
        }
        // Skip the bound address; the RFB stream starts right after it.
        switch reply[reply.startIndex + 3] {
        case 1: _ = try await read(4 + 2)
        case 4: _ = try await read(16 + 2)
        case 3: _ = try await read(Int(try await read(1)[0]) + 2)
        default: throw RFBError.transport("bad SOCKS reply")
        }
    }

    /// Reads exactly `count` bytes, so no RFB bytes are swallowed.
    private func read(_ count: Int) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            connection.receive(minimumIncompleteLength: count, maximumLength: count) { data, _, _, error in
                if let error {
                    continuation.resume(throwing: RFBError.transport("proxy: \(error)"))
                } else if let data, data.count == count {
                    continuation.resume(returning: data)
                } else {
                    continuation.resume(throwing: RFBError.connectionClosed)
                }
            }
        }
    }

    // MARK: RFBTransport (as swift-rfb's RFBTCPTransport)

    public func receive(expecting count: Int) async throws -> Data? {
        try await withCheckedThrowingContinuation { continuation in
            connection.receive(minimumIncompleteLength: min(max(count, 1), 1 << 20), maximumLength: max(count, 1 << 20)) {
                data, _, isComplete, error in
                if let data, !data.isEmpty {
                    continuation.resume(returning: data)
                } else if case .posix(.ECANCELED) = error {
                    continuation.resume(throwing: RFBError.connectionClosed)
                } else if let error {
                    continuation.resume(throwing: RFBError.transport("\(error)"))
                } else if isComplete {
                    continuation.resume(returning: nil)
                } else {
                    continuation.resume(returning: Data())
                }
            }
        }
    }

    public func send(_ data: Data) {
        connection.send(content: data, completion: .idempotent)
    }

    public func close() {
        connection.cancel()
    }
}

/// Runs the RFB session on the connection's queue (saves a thread hop per read).
final class QueueExecutor: TaskExecutor {
    let queue: DispatchQueue

    init(_ queue: DispatchQueue) {
        self.queue = queue
    }

    func enqueue(_ job: consuming ExecutorJob) {
        let job = UnownedJob(job)
        let executor = asUnownedTaskExecutor()
        queue.async { job.runSynchronously(on: executor) }
    }
}
