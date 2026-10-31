import Foundation
import Observation
import TailscaleKit

/// Drops TailscaleKit's logs (its BlackholeLogger has no public initializer).
struct QuietLogger: LogSink {
    var logFileHandle: Int32? { nil }
    func log(_ message: String) {}
}

/// The app's own Tailscale node (TailscaleKit, userspace; no VPN profile and no
/// Tailscale app needed). Signs in as the user, finds their cloud desktops,
/// and hands out a URLSession that reaches them over the tailnet.
@MainActor
@Observable
public final class TailnetService {
    public enum Phase: Equatable, Sendable {
        case stopped
        case starting
        /// Waiting for the user to sign in; the URL arrives shortly after start.
        case needsLogin(URL?)
        case running
        case failed(String)
    }

    public private(set) var phase: Phase = .stopped
    public private(set) var status: TailnetStatus?
    /// Requests through the tailnet (SOCKS5 proxy into the node). Set while running.
    public private(set) var session: URLSession?

    private let hostName: String
    private let stateDirectory: URL
    private var node: TailscaleNode?
    private var poller: Task<Void, Never>?

    public init(hostName: String) {
        self.hostName = hostName
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        stateDirectory = support.appendingPathComponent("tailscale", isDirectory: true)
    }

    public var desktops: [TailnetStatus.Peer] { status?.desktops ?? [] }

    public func start() {
        guard node == nil else { return }
        phase = .starting
        do {
            try FileManager.default.createDirectory(at: stateDirectory, withIntermediateDirectories: true)
            let config = Configuration(
                hostName: hostName,
                path: stateDirectory.path,
                authKey: nil,  // sign in through the browser
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
    /// start a fresh node (same identity) when the app comes back.
    public func restart() async {
        await stop()
        start()
    }

    /// Forgets this device's Tailscale identity; the next start asks to sign in.
    public func logOut() async {
        await stop()
        try? FileManager.default.removeItem(at: stateDirectory)
        status = nil
        start()
    }

    private func stop() async {
        poller?.cancel()
        poller = nil
        session = nil
        if let node {
            self.node = nil
            try? await node.close()
        }
        phase = .stopped
    }

    private func poll(_ node: TailscaleNode) async {
        while !Task.isCancelled, self.node === node {
            if let data = try? await node.statusJSON(),
                let status = try? JSONDecoder().decode(TailnetStatus.self, from: data)
            {
                self.status = status
                await apply(status, node: node)
            }
            let running = phase == .running
            try? await Task.sleep(for: running ? .seconds(15) : .seconds(1))
        }
    }

    private func apply(_ status: TailnetStatus, node: TailscaleNode) async {
        switch status.backendState {
        case "NeedsLogin", "NeedsMachineAuth":
            let url = status.authURL.flatMap { $0.isEmpty ? nil : URL(string: $0) }
            phase = .needsLogin(url)
        case "Running":
            if session == nil {
                do {
                    let (configuration, _) = try await URLSessionConfiguration.tailscaleSession(node)
                    configuration.timeoutIntervalForRequest = 30
                    session = URLSession(configuration: configuration)
                } catch {
                    phase = .failed("Tailscale proxy: \(error.localizedDescription)")
                    return
                }
            }
            phase = .running
        case "Stopped":
            phase = .failed("Tailscale stopped")
        default:
            if case .running = phase { return }
            phase = .starting
        }
    }
}
