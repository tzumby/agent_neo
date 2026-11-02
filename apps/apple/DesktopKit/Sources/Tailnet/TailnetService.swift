import Foundation
import Network
import Observation
import Synchronization
import TailscaleKit

/// Drops TailscaleKit's logs (its BlackholeLogger has no public initializer),
/// the Go node's included: -1 makes libtailscale discard them (TsnetSetLogFD).
/// With nil, the node would print to stderr (tsnet start-up, every closed
/// SOCKS5 connection, slow subscribers after the app was suspended).
struct QuietLogger: LogSink {
    var logFileHandle: Int32? { -1 }
    func log(_ message: String) {}
}

/// The app's own Tailscale node (TailscaleKit, userspace; no VPN profile and no
/// Tailscale app needed). Signs in as the user, or joins with a setup link's
/// key (a tester's device), finds their cloud desktops, and hands out a
/// URLSession that reaches them over the tailnet.
@MainActor
@Observable
public final class TailnetService {
    public enum Phase: Equatable, Sendable {
        case stopped
        case starting
        /// Waiting for the user to sign in; the URL arrives shortly after start.
        case needsLogin(URL?)
        /// Signed in, with the device list (so `desktops` is meaningful).
        case running
        /// A new node is starting after the app was suspended. The last session,
        /// proxy and desktops stay in place (connections retry) until it has
        /// the device list again.
        case reconnecting
        case failed(String)
    }

    public private(set) var phase: Phase = .stopped
    public private(set) var status: TailnetStatus?
    /// The user's desktops, as last seen while running (kept while reconnecting).
    public private(set) var desktops: [TailnetStatus.Peer] = []
    /// When the current node first said "Running". Right after start a node can
    /// be "Running" before it knows its peers, which isn't the same as having
    /// no desktop.
    private var runningSince: Date?
    /// Requests through the tailnet (SOCKS5 proxy into the node). Set while running.
    public private(set) var session: URLSession?
    /// The node's loopback SOCKS5 proxy, for raw connections such as VNC. Set while running.
    public private(set) var proxy: Proxy?
    /// Goes up each time `session` and `proxy` are replaced, so streams know to reconnect.
    public private(set) var generation = 0

    public struct Proxy: Sendable, Hashable {
        public let host: String
        public let port: UInt16
        public let username: String
        public let password: String
    }

    private let hostName: String
    private let stateDirectory: URL
    private var node: TailscaleNode?
    /// The node that `session` and `proxy` belong to.
    private var connectedNode: TailscaleNode?
    private var poller: Task<Void, Never>?

    public init(hostName: String) {
        self.hostName = hostName
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        stateDirectory = support.appendingPathComponent("tailscale", isDirectory: true)
    }

    public func start() {
        start(authKey: nil)
    }

    /// Joins with a setup link's key (`SetupLink`): forgets this device's
    /// current identity, then starts with the key. The key is used once; the
    /// node keeps its new identity from then on.
    public func join(authKey: String) async {
        await stop(keepingConnections: false)
        try? FileManager.default.removeItem(at: stateDirectory)
        status = nil
        start(authKey: authKey)
    }

    private func start(authKey: String?) {
        guard node == nil else { return }
        if phase != .reconnecting { phase = .starting }
        do {
            try FileManager.default.createDirectory(at: stateDirectory, withIntermediateDirectories: true)
            let config = Configuration(
                hostName: hostName,
                path: stateDirectory.path,
                authKey: authKey,  // nil: sign in through the browser
                controlURL: kDefaultControlURL,
                ephemeral: false
            )
            let node = try TailscaleNode(config: config, logger: QuietLogger())
            self.node = node
            // up() returns only once the node is signed in and running.
            Task {
                do {
                    try await node.up()
                } catch {
                    if self.node === node { self.phase = .failed(error.localizedDescription) }
                }
            }
            poller = Task { await self.poll(node) }
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// iOS takes the node's loopback proxy away while the app is suspended, so
    /// start a fresh node (same identity) when the app comes back. Views stay
    /// up meanwhile: the phase is `reconnecting` until the new node runs.
    public func restart() async {
        let reconnecting = phase == .running || phase == .reconnecting
        if reconnecting { phase = .reconnecting }
        await stop(keepingConnections: reconnecting)
        start()
    }

    /// After the app returns to the foreground: restarts only if the node's
    /// proxy no longer accepts connections (the app was suspended).
    public func ensureConnected() async {
        guard phase == .running, let proxy else { return }
        if await Self.accepts(proxy) { return }
        await restart()
    }

    /// Forgets this device's Tailscale identity; the next start asks to sign in.
    public func logOut() async {
        await stop(keepingConnections: false)
        try? FileManager.default.removeItem(at: stateDirectory)
        status = nil
        start()
    }

    private func stop(keepingConnections: Bool) async {
        poller?.cancel()
        poller = nil
        runningSince = nil
        if !keepingConnections {
            session = nil
            proxy = nil
            connectedNode = nil
            desktops = []
            phase = .stopped
        }
        if let node {
            self.node = nil
            try? await node.close()
        }
    }

    private func poll(_ node: TailscaleNode) async {
        while !Task.isCancelled, self.node === node {
            // Check the node again after each wait: a restart may have replaced
            // it, and a stale status must not touch the new node's state.
            if let data = try? await node.statusJSON(), self.node === node,
                let status = try? JSONDecoder().decode(TailnetStatus.self, from: data)
            {
                self.status = status
                await apply(status, node: node)
            }
            // Poll quickly until running with the device list, then rarely.
            try? await Task.sleep(for: phase == .running ? .seconds(15) : .seconds(1))
        }
    }

    private func apply(_ status: TailnetStatus, node: TailscaleNode) async {
        switch status.backendState {
        case "NeedsLogin", "NeedsMachineAuth":
            let url = status.authURL.flatMap { $0.isEmpty ? nil : URL(string: $0) }
            phase = .needsLogin(url)
        case "Running":
            // Wait for the device list. Any peer means it arrived; with none,
            // give it a few seconds (a tailnet with nothing else on it). After a
            // restart, wait however long it takes: an empty list would replace
            // the desktop with "No desktop yet", and the chat keeps retrying.
            if runningSince == nil { runningSince = .now }
            let hasPeers = !(status.peers ?? [:]).isEmpty
            let waited = Date.now.timeIntervalSince(runningSince ?? .now) > 8
            guard hasPeers || (waited && desktops.isEmpty) else {
                if case .needsLogin = phase { phase = .starting }  // just signed in
                return
            }
            if connectedNode !== node {
                do {
                    let (configuration, loopback) = try await URLSessionConfiguration.tailscaleSession(node)
                    guard self.node === node else { return }
                    configuration.timeoutIntervalForRequest = 30
                    session?.invalidateAndCancel()
                    session = URLSession(configuration: configuration)
                    proxy = Proxy(
                        host: loopback.ip ?? "127.0.0.1", port: UInt16(loopback.port ?? 0),
                        username: "tsnet", password: loopback.proxyCredential)
                    connectedNode = node
                    generation += 1
                } catch {
                    guard self.node === node else { return }
                    phase = .failed("Tailscale proxy: \(error.localizedDescription)")
                    return
                }
            }
            desktops = status.desktops
            phase = .running
        case "Stopped":
            phase = .failed("Tailscale stopped")
        default:
            // Starting up, or briefly without a connection: keep what's shown.
            if phase == .running || phase == .reconnecting { return }
            phase = .starting
        }
    }

    /// Whether something accepts TCP connections at the proxy's address within a second.
    private static func accepts(_ proxy: Proxy) async -> Bool {
        guard let port = NWEndpoint.Port(rawValue: proxy.port) else { return false }
        let connection = NWConnection(host: NWEndpoint.Host(proxy.host), port: port, using: .tcp)
        let answered = Mutex(false)
        return await withCheckedContinuation { continuation in
            let finish: @Sendable (Bool) -> Void = { ok in
                guard answered.withLock({ done in defer { done = true }; return !done }) else { return }
                connection.cancel()
                continuation.resume(returning: ok)
            }
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready: finish(true)
                case .failed, .waiting, .cancelled: finish(false)
                default: break
                }
            }
            connection.start(queue: .global(qos: .userInitiated))
            DispatchQueue.global().asyncAfter(deadline: .now() + 1) { finish(false) }
        }
    }
}
