import Foundation

/// The parts of Tailscale's status document (ipnstate.Status, as served by
/// LocalAPI's /status) that the apps use. Decoded here rather than with
/// TailscaleKit's own types, which lag behind the Go side.
public struct TailnetStatus: Decodable, Sendable {
    public let backendState: String
    public let authURL: String?
    public let selfNode: Peer?
    public let peers: [String: Peer]?
    public let currentTailnet: Tailnet?
    public let users: [String: UserProfile]?

    enum CodingKeys: String, CodingKey {
        case backendState = "BackendState"
        case authURL = "AuthURL"
        case selfNode = "Self"
        case peers = "Peer"
        case currentTailnet = "CurrentTailnet"
        case users = "User"
    }

    public struct Peer: Decodable, Sendable, Identifiable, Hashable {
        public let id: String
        public let hostName: String
        public let dnsName: String
        public let online: Bool?
        public let tags: [String]?
        public let userID: Int64?
        public let tailscaleIPs: [String]?

        enum CodingKeys: String, CodingKey {
            case id = "ID"
            case hostName = "HostName"
            case dnsName = "DNSName"
            case online = "Online"
            case tags = "Tags"
            case userID = "UserID"
            case tailscaleIPs = "TailscaleIPs"
        }

        /// The peer's tailnet IPv4 address (100.x.y.z).
        public var ipv4: String? { tailscaleIPs?.first { !$0.contains(":") } }

        /// MagicDNS name without the trailing dot, e.g. neo-agent.tail57aeb.ts.net.
        public var host: String {
            dnsName.hasSuffix(".") ? String(dnsName.dropLast()) : dnsName
        }
    }

    public struct Tailnet: Decodable, Sendable {
        public let name: String
        public let magicDNSSuffix: String?

        enum CodingKeys: String, CodingKey {
            case name = "Name"
            case magicDNSSuffix = "MagicDNSSuffix"
        }
    }

    public struct UserProfile: Decodable, Sendable {
        public let loginName: String
        public let displayName: String?

        enum CodingKeys: String, CodingKey {
            case loginName = "LoginName"
            case displayName = "DisplayName"
        }
    }

    /// Desktops: online or not, devices tagged tag:neo-agent (tailnet-proxy's tag).
    public var desktops: [Peer] {
        (peers?.values.map { $0 } ?? [])
            .filter { $0.tags?.contains("tag:neo-agent") == true }
            .sorted { $0.hostName < $1.hostName }
    }

    /// The Tailscale login this app is signed in as.
    public var loginName: String? {
        guard let id = selfNode?.userID else { return nil }
        return users?[String(id)]?.loginName
    }
}
